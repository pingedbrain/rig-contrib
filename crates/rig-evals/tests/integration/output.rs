//! The report as JSON, JUnit XML and Markdown, and the sinks writing them.

use rig_evals::{
    Case, Experiment, JsonFile, JunitFile, Markdown, Report, TargetError, TargetOutput, evaluators,
    target_fn,
};

use crate::tokens;

/// One case passes; the other answers `<nothing & more>` and fails.
async fn report(experiment: Experiment) -> Report {
    experiment
        .dataset(vec![
            Case::new("good", "say ok"),
            Case::new("bad", "say <nothing & more>"),
        ])
        .target(target_fn(|case: Case| async move {
            let output = if case.id == "good" {
                "ok".to_owned()
            } else {
                case.input
            };
            Ok::<_, TargetError>(TargetOutput::new(output).with_usage(tokens(10, 2)))
        }))
        .evaluator(evaluators::contains("ok"))
        .evaluator(evaluators::no_error())
        .run()
        .await
        .expect("runs")
}

/// Whether `xml` is well-formed: it tokenizes and its elements nest.
fn assert_well_formed(xml: &str) {
    let mut open: Vec<String> = Vec::new();
    for token in xmlparser::Tokenizer::from(xml) {
        match token.expect("valid XML") {
            xmlparser::Token::ElementStart { local, .. } => open.push(local.to_string()),
            xmlparser::Token::ElementEnd { end, .. } => match end {
                xmlparser::ElementEnd::Open => {}
                xmlparser::ElementEnd::Close(_, local) => {
                    assert_eq!(
                        open.pop().as_deref(),
                        Some(local.as_str()),
                        "mismatched tag"
                    );
                }
                xmlparser::ElementEnd::Empty => {
                    open.pop();
                }
            },
            _ => {}
        }
    }
    assert!(open.is_empty(), "unclosed {open:?}");
}

#[tokio::test]
async fn junit_has_a_test_case_per_trial_and_threshold_with_the_failures() {
    let report = report(Experiment::new("support <bot>")).await;
    let xml = report.to_junit();
    assert_well_formed(&xml);

    // Two trials and two thresholds; the bad trial and contains' threshold fail.
    assert!(
        xml.contains(r#"<testsuite name="support &lt;bot&gt;" tests="4" failures="2""#),
        "{xml}"
    );
    assert!(xml.contains(r#"<testcase name="good" classname="support &lt;bot&gt;""#));
    assert!(xml.contains(r#"<failure message="failed: contains" type="evaluation">"#));
    assert!(xml.contains("missing [&quot;ok&quot;]"), "{xml}");
    assert!(xml.contains(r#"<testcase name="threshold contains""#));
    assert!(xml.contains(r#"<failure message="pass rate 0.50 below 1.00" type="threshold"/>"#));
}

#[tokio::test]
async fn json_round_trips() {
    let report = report(Experiment::new("support")).await;
    let json = report.to_json().expect("serializes");
    let value: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
    assert_eq!(value.pointer("/experiment"), Some(&"support".into()));
    assert_eq!(value.pointer("/tokens/total"), Some(&24.into()));
    assert!(
        value
            .pointer("/trials/0/trial/latency_ms")
            .is_some_and(serde_json::Value::is_f64)
    );
    assert_eq!(
        serde_json::from_str::<Report>(&json).expect("parses back"),
        report
    );
}

#[tokio::test]
async fn markdown_summarizes_and_lists_the_failures() {
    let markdown = report(Experiment::new("support")).await.to_markdown();
    assert!(markdown.starts_with("# support: FAILED"), "{markdown}");
    assert!(
        markdown.contains("| contains | 50% | 100% | 0.50 | 1/2 | FAIL |"),
        "{markdown}"
    );
    assert!(
        markdown.contains("| no_error | 100% | 100% | 1.00 | 2/2 | pass |"),
        "{markdown}"
    );
    assert!(markdown.contains("Tokens: 20 input, 4 output, 24 total."));
    assert!(markdown.contains("## Failures"));
    assert!(
        markdown.contains("- `bad` #1 contains: missing [\"ok\"]"),
        "{markdown}"
    );
}

#[tokio::test]
async fn file_sinks_write_into_new_directories() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = |name: &str| dir.path().join("out").join(name);
    let report = report(
        Experiment::new("support")
            .sink(JsonFile::new(path("report.json")))
            .sink(JunitFile::new(path("junit.xml")))
            .sink(Markdown::file(path("summary.md"))),
    )
    .await;

    let read = |name: &str| std::fs::read_to_string(path(name)).expect(name);
    assert_eq!(read("report.json"), report.to_json().expect("json"));
    assert_eq!(read("junit.xml"), report.to_junit());
    assert_eq!(read("summary.md"), report.to_markdown());
}
