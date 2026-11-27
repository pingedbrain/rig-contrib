//! Request-scoped run state: nodes and edge conditions read it, no model ever
//! sees it, and a resumed run must be given it again. Scripted models, no
//! network.

use std::panic::{RefUnwindSafe, UnwindSafe};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use rig_agent::AgentBuilder;
use rig_core::test_utils::{MockCompletionModel, MockTurn};
use rig_memory::MemoryStore;
use rig_multiagent::{
    Graph, GraphRun, GraphState, NodeError, RunState, node_fn, node_fn_with_state,
};

/// Compile-time check: the run types keep the auto traits that callers rely on
/// to hold them across `catch_unwind` and move them between tasks.
#[test]
fn run_types_keep_their_auto_traits() {
    fn assert_auto_traits<T: UnwindSafe + RefUnwindSafe + Send + Sync>() {}
    assert_auto_traits::<RunState>();
    assert_auto_traits::<GraphState>();
    assert_auto_traits::<GraphRun>();
}

const SECRET: &str = "tenant-secret-9f2";

struct Tenant(String);

struct Tier(u8);

/// A stand-in for a database handle: shared, not serializable.
struct Db(Arc<AtomicUsize>);

#[tokio::test]
async fn a_function_node_reads_the_run_state() {
    let graph = Graph::builder()
        .node(
            "greet",
            node_fn_with_state(|input: String, state: RunState| async move {
                let Some(tenant) = state.get::<Tenant>() else {
                    return Err(NodeError::from("no tenant"));
                };
                Ok(format!("{}: {input}", tenant.0))
            }),
        )
        .build()
        .expect("the builder is valid");

    let run = graph
        .run_with_state("hello", RunState::new().with(Tenant("acme".to_owned())))
        .await
        .expect("the run succeeds");
    assert_eq!(run.answer(), "acme: hello");

    let error = graph
        .run("hello")
        .await
        .expect_err("a plain run has no state");
    assert!(error.to_string().contains("no tenant"), "{error}");
}

#[tokio::test]
async fn an_edge_condition_routes_on_the_run_state() {
    let graph = Graph::builder()
        .node(
            "classify",
            node_fn(|_: String| async { Ok("classified".to_owned()) }),
        )
        .node(
            "premium",
            node_fn(|_: String| async { Ok("premium".to_owned()) }),
        )
        .node(
            "basic",
            node_fn(|_: String| async { Ok("basic".to_owned()) }),
        )
        .edge_if("classify", "premium", |state: &GraphState| {
            state
                .run_state()
                .get::<Tier>()
                .is_some_and(|tier| tier.0 >= 2)
        })
        .edge_if("classify", "basic", |state: &GraphState| {
            state
                .run_state()
                .get::<Tier>()
                .is_none_or(|tier| tier.0 < 2)
        })
        .build()
        .expect("the builder is valid");

    let gold = graph
        .run_with_state("upgrade", RunState::new().with(Tier(2)))
        .await
        .expect("the run succeeds");
    assert_eq!(gold.state.order(), ["classify", "premium"]);
    assert_eq!(gold.answer(), "premium");

    let free = graph
        .run_with_state("upgrade", RunState::new().with(Tier(1)))
        .await
        .expect("the run succeeds");
    assert_eq!(free.state.order(), ["classify", "basic"]);
}

#[tokio::test]
async fn a_nested_graph_gets_the_run_state_of_its_outer_run() {
    let inner = Graph::builder()
        .node(
            "inner",
            node_fn_with_state(|_: String, state: RunState| async move {
                let tenant = state
                    .get::<Tenant>()
                    .map_or("none", |tenant| tenant.0.as_str());
                Ok(tenant.to_owned())
            }),
        )
        .build()
        .expect("the builder is valid");
    let outer = Graph::builder()
        .node("outer", inner)
        .build()
        .expect("the builder is valid");

    let run = outer
        .run_with_state("task", RunState::new().with(Tenant("acme".to_owned())))
        .await
        .expect("the run succeeds");
    assert_eq!(run.answer(), "acme");
}

#[tokio::test]
async fn the_run_state_never_reaches_a_model() {
    let model = MockCompletionModel::from_turns([MockTurn::text("Here are the invoices.")]);
    let billing = AgentBuilder::new(model.clone()).name("billing").build();
    let graph = Graph::builder()
        .node(
            "scope",
            node_fn_with_state(|_: String, state: RunState| async move {
                // The function reads the state and passes on only the text it chooses.
                if state.get::<Tenant>().is_none() {
                    return Err(NodeError::from("no tenant"));
                }
                Ok("List the open invoices.".to_owned())
            }),
        )
        .node("billing", billing)
        .edge_if("scope", "billing", |state: &GraphState| {
            state.run_state().get::<Tenant>().is_some()
        })
        .build()
        .expect("the builder is valid");

    let run = graph
        .run_with_state(
            "Show my bills",
            RunState::new().with(Tenant(SECRET.to_owned())),
        )
        .await
        .expect("the run succeeds");
    assert_eq!(run.answer(), "Here are the invoices.");

    let sent = serde_json::to_string(
        &model
            .requests()
            .iter()
            .map(|request| &request.chat_history)
            .collect::<Vec<_>>(),
    )
    .expect("the prompts serialize");
    assert!(sent.contains("List the open invoices."), "{sent}");
    assert!(
        !sent.contains(SECRET),
        "the state reached the model: {sent}"
    );
}

#[tokio::test]
async fn a_resumed_run_is_given_its_state_again() {
    let writes = Arc::new(AtomicUsize::new(0));
    let graph = Graph::builder()
        .node(
            "plan",
            node_fn(|_: String| async { Ok("the plan".to_owned()) }),
        )
        .node(
            "save",
            node_fn_with_state(|input: String, state: RunState| async move {
                let Some(db) = state.get::<Db>() else {
                    return Err(NodeError::from("no database"));
                };
                // The first write fails; the second one works.
                if db.0.fetch_add(1, Ordering::SeqCst) == 0 {
                    return Err(NodeError::from("database is down"));
                }
                Ok(format!("saved {input}"))
            }),
        )
        .edge("plan", "save")
        .checkpoints(MemoryStore::default())
        .build()
        .expect("the builder is valid");
    let with_db = || RunState::new().with(Db(writes.clone()));

    let failed = graph
        .run_as_with_state("job-1", "ship it", with_db())
        .await
        .expect_err("the first write fails");
    assert!(failed.to_string().contains("database is down"), "{failed}");

    // The checkpoint does not keep the state: resumed without it, the node
    // finds no database and fails again.
    let stateless = graph
        .run_as("job-1", "ship it")
        .await
        .expect_err("the state is not in the checkpoint");
    assert!(stateless.to_string().contains("no database"), "{stateless}");

    let resumed = graph
        .run_as_with_state("job-1", "ignored on resume", with_db())
        .await
        .expect("the resumed run succeeds");
    assert_eq!(
        resumed.output("save"),
        Some("saved Original task: ship it\n\nInputs from previous nodes:\n\nFrom plan:\nthe plan")
    );
    assert_eq!(resumed.state.runs("plan"), 1, "the plan is not run again");
    assert_eq!(writes.load(Ordering::SeqCst), 2);
}
