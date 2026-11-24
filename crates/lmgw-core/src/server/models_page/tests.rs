use super::*;

const IDS: [&str; 5] = ["a", "b", "c", "d", "e"];

fn ask(q: &str) -> Result<Page, GatewayError> {
    let uri: Uri = format!("/v1/models{q}").parse().unwrap();
    page_of(&uri, &IDS)
}

fn page(start: usize, end: usize, has_more: bool) -> Page {
    Page {
        start,
        end,
        has_more,
    }
}

#[test]
fn no_query_is_the_first_page_of_the_default_size() {
    let ids: Vec<String> = (0..30).map(|i| format!("m{i:02}")).collect();
    let refs: Vec<&str> = ids.iter().map(String::as_str).collect();
    let uri: Uri = "/v1/models".parse().unwrap();
    assert_eq!(page_of(&uri, &refs).unwrap(), page(0, 20, true));
}

#[test]
fn a_short_list_fits_one_page() {
    assert_eq!(ask("").unwrap(), page(0, 5, false));
    assert_eq!(ask("?limit=5").unwrap(), page(0, 5, false));
    assert_eq!(ask("?limit=2").unwrap(), page(0, 2, true));
}

#[test]
fn after_id_follows_the_cursor() {
    assert_eq!(ask("?limit=2&after_id=b").unwrap(), page(2, 4, true));
    assert_eq!(ask("?limit=2&after_id=c").unwrap(), page(3, 5, false));
    assert_eq!(ask("?after_id=e").unwrap(), page(5, 5, false));
}

#[test]
fn before_id_precedes_the_cursor() {
    assert_eq!(ask("?limit=2&before_id=e").unwrap(), page(2, 4, true));
    assert_eq!(ask("?limit=2&before_id=c").unwrap(), page(0, 2, false));
    assert_eq!(ask("?limit=3&before_id=b").unwrap(), page(0, 1, false));
    assert_eq!(ask("?before_id=a").unwrap(), page(0, 0, false));
}

#[test]
fn bad_limits_are_refused() {
    for q in [
        "limit=0",
        "limit=1001",
        "limit=-1",
        "limit=x",
        "limit=",
        "limit=1.5",
    ] {
        let err = ask(&format!("?{q}")).unwrap_err();
        assert!(err.to_string().contains("limit"), "{q}: {err}");
    }
    assert!(ask("?limit=1000").is_ok());
}

#[test]
fn both_cursors_are_refused() {
    let err = ask("?after_id=a&before_id=c").unwrap_err();
    assert!(err.to_string().contains("together"), "{err}");
}

#[test]
fn an_unknown_cursor_is_refused_by_name() {
    let err = ask("?after_id=zz").unwrap_err();
    assert!(err.to_string().contains("after_id 'zz'"), "{err}");
    let err = ask("?before_id=zz").unwrap_err();
    assert!(err.to_string().contains("before_id 'zz'"), "{err}");
}

#[test]
fn a_cursor_with_slashes_and_escapes_is_found() {
    let ids = ["kilo/anthropic/x", "kilo/y"];
    let uri: Uri = "/v1/models?after_id=kilo%2Fanthropic%2Fx".parse().unwrap();
    assert_eq!(page_of(&uri, &ids).unwrap(), page(1, 2, false));
}
