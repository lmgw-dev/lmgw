use serde_json::json;

use super::*;

fn split(total: u64, hour: Option<u64>) -> Usage {
    Usage {
        prompt_tokens: Some(total + 10),
        completion_tokens: Some(3),
        cache_write_tokens: Some(total),
        cache_write_1h_tokens: hour,
        ..Usage::default()
    }
}

#[test]
fn the_object_is_read_with_its_total() {
    let u = json!({
        "cache_creation_input_tokens": 500,
        "cache_creation": {"ephemeral_5m_input_tokens": 200, "ephemeral_1h_input_tokens": 300},
    });
    assert_eq!((total(&u), one_hour(&u)), (Some(500), Some(300)));
    // No object: no split, though the total is known.
    let flat = json!({"cache_creation_input_tokens": 500});
    assert_eq!((total(&flat), one_hour(&flat)), (Some(500), None));
    // An object without its 1-hour member reports none written so.
    let five = json!({"cache_creation": {"ephemeral_5m_input_tokens": 40}});
    assert_eq!((total(&five), one_hour(&five)), (Some(40), Some(0)));
    assert_eq!((total(&json!({})), one_hour(&json!({}))), (None, None));
}

#[test]
fn the_wire_object_is_written_only_for_a_known_split() {
    assert_eq!(
        creation_object(&split(500, Some(300))),
        Some(json!({"ephemeral_5m_input_tokens": 200, "ephemeral_1h_input_tokens": 300}))
    );
    assert_eq!(
        creation_object(&split(500, Some(0))),
        Some(json!({"ephemeral_5m_input_tokens": 500, "ephemeral_1h_input_tokens": 0}))
    );
    assert_eq!(creation_object(&split(500, None)), None);
}

/// The 1-hour part is of the input group: a later report with an input
/// total replaces it with the cache counters (latest wins, `merge`), a
/// report without one leaves it; across separate calls it sums (`add`).
#[test]
fn merge_moves_it_with_the_input_group_and_add_sums_it() {
    let mut u = split(500, Some(300));
    u.merge(&Usage {
        completion_tokens: Some(9),
        ..Usage::default()
    });
    assert_eq!(u.cache_write_1h_tokens, Some(300));
    u.merge(&split(100, None));
    assert_eq!(u.cache_write_1h_tokens, None);

    let mut sum = split(500, Some(300));
    sum.add(&split(100, Some(0)));
    assert_eq!(
        (sum.cache_write_tokens, sum.cache_write_1h_tokens),
        (Some(600), Some(300))
    );
    sum.add(&split(50, None));
    assert_eq!(
        (sum.cache_write_tokens, sum.cache_write_1h_tokens),
        (Some(650), Some(300))
    );
}
