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

#[test]
fn a_repeated_id_loads_with_every_case() {
    // Loading stays permissive (a warning names both places); the experiment
    // is the gate, so the existing refusal still holds.
    let dataset = Dataset::from_jsonl(
        "{\"id\": \"a\", \"input\": \"1\"}\n{\"id\": \"b\", \"input\": \"2\"}\n{\"id\": \"a\", \"input\": \"3\"}\n",
    )
    .expect("a repeated id is not a parse error");
    let ids: Vec<&str> = dataset.cases.iter().map(|case| case.id.as_str()).collect();
    assert_eq!(ids, ["a", "b", "a"]);

    let dataset = Dataset::from_json(r#"[{"id": "a", "input": "1"}, {"id": "a", "input": "2"}]"#)
        .expect("a repeated id is not a parse error");
    assert_eq!(dataset.cases.len(), 2);
}

#[test]
fn a_bad_jsonl_line_is_named_with_the_path_and_line() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("bad.jsonl");
    std::fs::write(
        &path,
        "{\"id\": \"a\", \"input\": \"1\"}\n\n{\"id\": \"b\", \"input\": \"2\"}\n{\"id\": \"c\", \"input\": \"x\", \"turns\": [\"y\"]}\n",
    )
    .expect("write");

    let error = Dataset::load(&path).expect_err("line 4 sets both input and turns");
    let message = error.to_string();
    assert!(
        message.contains(&path.display().to_string()),
        "no path in {message:?}"
    );
    assert!(message.contains("line 4"), "no line 4 in {message:?}");
    assert!(
        matches!(&error, DatasetError::InFile { source, .. }
            if matches!(**source, DatasetError::Line { line: 4, .. })),
        "{error:?}"
    );
}

#[test]
fn a_bad_json_array_element_is_named_with_its_index_and_path() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("bad.json");
    std::fs::write(
        &path,
        r#"[{"id": "a", "input": "1"}, {"id": "b"}, {"id": "c", "input": "3"}]"#,
    )
    .expect("write");

    let error = Dataset::load(&path).expect_err("element 1 has no input");
    let message = error.to_string();
    assert!(
        message.contains(&path.display().to_string()),
        "no path in {message:?}"
    );
    assert!(message.contains("index 1"), "no index 1 in {message:?}");

    // Without a path, the index alone is named.
    let error = Dataset::from_json(r#"[{"id": "a", "input": "1"}, 7]"#).expect_err("not a case");
    assert!(
        matches!(error, DatasetError::Element { index: 1, .. }),
        "{error}"
    );
}

#[test]
fn a_malformed_json_file_names_the_path() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("broken.json");
    std::fs::write(&path, "{\"id\": \"a\"}").expect("write");

    let message = Dataset::load(&path)
        .expect_err("an object, not an array")
        .to_string();
    assert!(
        message.contains(&path.display().to_string()),
        "no path in {message:?}"
    );
}
