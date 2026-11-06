//! Which image row runs an upscale (chat-media-tools design §9, the owner's
//! answer of 2026-10-21): a resident row whose capabilities report
//! `upscale`, preferring the row that made the picture; with none resident,
//! the row that made it; for a picture no row made (an upload, a cloud
//! picture), the enabled row with the smallest planning footprint.
//!
//! Pure over what the caller read, so the rule is tested without a gateway.

/// One image row that may run it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Candidate {
    pub model_id: String,
    /// Its container is up and ready.
    pub resident: bool,
    /// What that container's capabilities say about `upscale`; `None` when
    /// it is not resident or reported none.
    pub upscale: Option<bool>,
    /// Its planning footprint in bytes, when lmgw can size it.
    pub footprint: Option<u64>,
}

/// The row to run on, by model id; `None` when there is no candidate.
pub(crate) fn choose<'a>(candidates: &'a [Candidate], made_by: Option<&str>) -> Option<&'a str> {
    let maker = made_by.and_then(|m| candidates.iter().find(|c| c.model_id == m));
    let ready: Vec<&Candidate> = candidates
        .iter()
        .filter(|c| c.resident && c.upscale == Some(true))
        .collect();
    if !ready.is_empty() {
        if let Some(m) = maker.filter(|m| ready.iter().any(|r| r.model_id == m.model_id)) {
            return Some(&m.model_id);
        }
        return smallest(ready.into_iter());
    }
    if let Some(m) = maker {
        return Some(&m.model_id);
    }
    smallest(candidates.iter())
}

/// The candidate with the smallest known footprint; one lmgw cannot size
/// comes after every one it can, and the model id breaks a tie.
fn smallest<'a>(c: impl Iterator<Item = &'a Candidate>) -> Option<&'a str> {
    c.min_by(|a, b| {
        let key = |x: &Candidate| (x.footprint.is_none(), x.footprint.unwrap_or(0));
        key(a)
            .cmp(&key(b))
            .then_with(|| a.model_id.cmp(&b.model_id))
    })
    .map(|c| c.model_id.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(id: &str, resident: bool, upscale: Option<bool>, footprint: Option<u64>) -> Candidate {
        Candidate {
            model_id: id.into(),
            resident,
            upscale,
            footprint,
        }
    }

    #[test]
    fn a_resident_upscaler_wins_and_the_maker_among_them_first() {
        let rows = vec![
            c("big", true, Some(true), Some(20)),
            c("small", true, Some(true), Some(5)),
            c("maker", false, None, Some(1)),
        ];
        assert_eq!(choose(&rows, Some("maker")), Some("small"));
        assert_eq!(choose(&rows, Some("big")), Some("big"));
        assert_eq!(choose(&rows, None), Some("small"));
    }

    #[test]
    fn with_none_resident_the_maker_runs_it_else_the_smallest_row() {
        let rows = vec![
            c("a", true, Some(false), Some(3)),
            c("b", false, None, Some(9)),
            c("c", false, None, None),
        ];
        assert_eq!(choose(&rows, Some("b")), Some("b"));
        assert_eq!(choose(&rows, None), Some("a"));
        assert_eq!(choose(&rows, Some("cloud-model")), Some("a"));
        let unsizable = vec![c("z", false, None, None), c("y", false, None, None)];
        assert_eq!(choose(&unsizable, None), Some("y"));
        assert_eq!(choose(&[], None), None);
    }
}
