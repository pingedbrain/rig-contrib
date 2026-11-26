//! Reading cases from JSON and JSON Lines.

use rig_evals::{Case, Dataset, DatasetError, ExpectedCall};
use serde_json::json;

#[test]
fn a_json_array_of_cases_parses_with_every_reference() {
    let dataset = Dataset::from_json(
        r#"[
            {"id": "refund", "input": "Can I get a refund?", "expected": "Yes",
             "trajectory": [{"name": "lookup_order", "args": {"id": 7}}, {"name": "refund"}],
             "assertions": ["mentions the 30-day window"],
             "metadata": {"tier": "gold"}},
            {"id": "hello", "input": "Hi"}
        ]"#,
    )
    .expect("valid");

    assert_eq!(dataset.cases.len(), 2);
    let refund = dataset.cases.first().expect("case 0");
    assert_eq!(
        refund,
        &Case::new("refund", "Can I get a refund?")
            .expected("Yes")
            .trajectory([
                ExpectedCall::new("lookup_order").args(json!({"id": 7})),
                ExpectedCall::new("refund"),
            ])
            .assertion("mentions the 30-day window")
            .metadata("tier", "gold")
    );
    assert_eq!(dataset.cases.get(1), Some(&Case::new("hello", "Hi")));
}

#[test]
fn json_lines_skip_blank_lines_and_name_the_bad_one() {
    let dataset = Dataset::from_jsonl(
        "{\"id\": \"a\", \"input\": \"1\"}\n\n{\"id\": \"b\", \"input\": \"2\"}\n",
    )
    .expect("valid");
    let ids: Vec<&str> = dataset.cases.iter().map(|case| case.id.as_str()).collect();
    assert_eq!(ids, ["a", "b"]);

    let error = Dataset::from_jsonl("{\"id\": \"a\", \"input\": \"1\"}\n{\"id\": \"b\"}\n")
        .expect_err("line 2 has no input");
    assert!(
        matches!(error, DatasetError::Line { line: 2, .. }),
        "{error}"
    );
}

#[test]
fn load_picks_the_format_by_extension() {
    let dir = tempfile::tempdir().expect("tempdir");
    let json = dir.path().join("cases.json");
    std::fs::write(&json, r#"[{"id": "a", "input": "1"}]"#).expect("write");
    let jsonl = dir.path().join("cases.jsonl");
    std::fs::write(&jsonl, "{\"id\": \"a\", \"input\": \"1\"}\n").expect("write");

    assert_eq!(
        Dataset::load(&json).expect("json"),
        Dataset::load(&jsonl).expect("jsonl")
    );
    assert!(matches!(
        Dataset::load(dir.path().join("cases.csv")),
        Err(DatasetError::UnknownFormat(_))
    ));
    assert!(matches!(
        Dataset::load(dir.path().join("missing.json")),
        Err(DatasetError::Read { .. })
    ));
}
