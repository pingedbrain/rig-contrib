use super::*;

#[tokio::test]
async fn in_memory_forgets_the_oldest_context_past_the_limit() {
    let store = InMemory::new(2);
    let hi = || vec![Message::user("hi")];
    store.append("a", hi()).await.unwrap();
    store.append("b", hi()).await.unwrap();
    store.append("a", hi()).await.unwrap(); // an existing context does not count again
    assert_eq!(store.load("a").await.unwrap().len(), 2);
    store.append("c", hi()).await.unwrap();
    assert!(
        store.load("a").await.unwrap().is_empty(),
        "oldest context forgotten"
    );
    assert_eq!(store.load("b").await.unwrap().len(), 1);
}

#[tokio::test]
async fn a_rig_memory_store_keeps_conversations_across_handles() {
    let shared = rig_memory::MemoryStore::default();
    let one = Persistent(shared.clone());
    one.append(
        "ctx",
        vec![Message::user("hi"), Message::assistant("hello")],
    )
    .await
    .unwrap();
    let other = Persistent(shared); // a second server on the same store
    other
        .append("ctx", vec![Message::user("again")])
        .await
        .unwrap();
    assert_eq!(one.load("ctx").await.unwrap().len(), 3);
    assert!(one.load("new").await.unwrap().is_empty());
}

#[test]
fn context_ids_that_could_escape_a_file_store_are_refused() {
    assert!(valid_context_id("019a2b3c-4d5e-7f00-8a9b-0c1d2e3f4a5b"));
    assert!(valid_context_id("user_42"));
    for bad in [
        "",
        "../etc/passwd",
        "a/b",
        "..",
        "a.b",
        "con text",
        &"x".repeat(129),
    ] {
        assert!(!valid_context_id(bad), "{bad:?}");
    }
}
