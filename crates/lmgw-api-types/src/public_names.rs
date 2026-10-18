//! The names clients call local models by: a class's public prefix and the
//! model id, and which of the aux class's two prefixes a row's kind takes
//! (decision-models design §3.10). Shared so the aux editor names a draft as
//! the gateway will name the saved row.

/// `id` under `prefix`, the prefix trimmed of its slashes; an empty prefix
/// gives the bare id.
pub fn prefixed(prefix: &str, id: &str) -> String {
    match prefix.trim_matches('/') {
        "" => id.to_string(),
        p => format!("{p}/{id}"),
    }
}

/// The prefix an aux row of `kind` (`embed`, `rerank` or `decision`) is
/// named under: the decision prefix for a decision row, the class's public
/// prefix for the others.
pub fn aux_prefix<'a>(kind: &str, public_prefix: &'a str, decision_prefix: &'a str) -> &'a str {
    match kind {
        "decision" => decision_prefix,
        _ => public_prefix,
    }
}

/// The route an aux row of `kind` answers on.
pub fn aux_endpoint(kind: &str) -> &'static str {
    match kind {
        "decision" => "/v1/systemone",
        "rerank" => "/v1/rerank",
        _ => "/v1/embeddings",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_prefix_is_trimmed_of_its_slashes_and_an_empty_one_gives_the_bare_id() {
        assert_eq!(prefixed("embed/", "bge"), "embed/bge");
        assert_eq!(prefixed("/audio/", "tts"), "audio/tts");
        assert_eq!(prefixed("//a//b//", "c1"), "a//b/c1");
        assert_eq!(prefixed("", "c1"), "c1");
        assert_eq!(prefixed("/", "img"), "img");
    }

    #[test]
    fn a_decision_row_takes_the_decision_prefix_and_its_route() {
        assert_eq!(aux_prefix("decision", "embed", "decide"), "decide");
        assert_eq!(aux_prefix("rerank", "embed", "decide"), "embed");
        assert_eq!(aux_prefix("embed", "embed", "decide"), "embed");
        assert_eq!(aux_endpoint("decision"), "/v1/systemone");
        assert_eq!(aux_endpoint("rerank"), "/v1/rerank");
        assert_eq!(aux_endpoint("embed"), "/v1/embeddings");
    }
}
