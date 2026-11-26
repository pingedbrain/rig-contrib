use std::time::Duration;

use super::{escape, percentile};

#[test]
fn percentiles_use_the_nearest_rank() {
    let sorted: Vec<Duration> = (1..=20).map(Duration::from_millis).collect();
    assert_eq!(percentile(&sorted, 0.50), Duration::from_millis(10));
    assert_eq!(percentile(&sorted, 0.95), Duration::from_millis(19));
    assert_eq!(
        percentile(sorted.get(..1).unwrap(), 0.95),
        Duration::from_millis(1)
    );
    assert_eq!(percentile(&[], 0.5), Duration::ZERO);
}

#[test]
fn xml_escaping_covers_markup_and_forbidden_characters() {
    assert_eq!(
        escape("a<b & \"c\" 'd'>\u{1}\n"),
        "a&lt;b &amp; &quot;c&quot; &apos;d&apos;&gt;\n"
    );
}
