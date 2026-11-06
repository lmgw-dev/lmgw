//! The words of a guest start that did not fit (candidate-aliases design
//! §4.3): what holds the card, said to read after "GPU in use by ".

use crate::hf::fmt_bytes;

use super::super::view::ResidentView;

/// What holds the memory a guest's start could not get, to read after "GPU
/// in use by ": every resident (none of them an idle guest any more — those
/// were evicted) and the VRAM in use outside lmgw (`outside`: the scheduler's
/// per-process measurement, the same figure admission's external shortfall
/// names — `None` when none could be taken, and then no clause is made up),
/// or the applications outside lmgw when nothing of lmgw's is on the card.
pub(super) fn in_use(
    residents: &[ResidentView],
    outside: Option<u64>,
    label: &str,
    needs: u64,
    free: u64,
) -> String {
    let who = if residents.is_empty() {
        match outside {
            Some(o) if o > 0 => format!("applications outside lmgw (about {})", fmt_bytes(o)),
            _ => "applications outside lmgw".to_string(),
        }
    } else {
        let mut parts: Vec<String> = residents
            .iter()
            .map(|r| {
                format!(
                    "{}/{} ({}{})",
                    r.container.as_str(),
                    r.model,
                    fmt_bytes(r.estimated_bytes),
                    if r.in_flight > 0 {
                        format!(", {} in flight", r.in_flight)
                    } else {
                        String::new()
                    }
                )
            })
            .collect();
        parts.sort();
        if let Some(o) = outside.filter(|o| *o > 0) {
            parts.push(format!(
                "applications outside lmgw (about {})",
                fmt_bytes(o)
            ));
        }
        parts.join(", ")
    };
    format!(
        "{who} — {label} needs {} and {} is free",
        fmt_bytes(needs),
        fmt_bytes(free)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::Class;

    const GIB: u64 = 1 << 30;

    fn resident(model: &str, gib: u64) -> ResidentView {
        ResidentView {
            container: Class::Chat,
            model: model.into(),
            state: "ready".into(),
            estimated_bytes: gib * GIB,
            in_flight: 0,
            idle_seconds: Some(0),
            peak_extra_bytes: None,
            pending_bytes: None,
            note: None,
        }
    }

    #[test]
    fn the_deferral_names_lmgws_models_and_the_vram_outside_it() {
        let said = in_use(
            &[resident("small", 3)],
            Some(6 * GIB),
            "chat/big",
            20 * GIB,
            15 * GIB,
        );
        assert_eq!(
            said,
            "chat/small (3.0 GiB), applications outside lmgw (about 6.0 GiB) — chat/big needs \
             20.0 GiB and 15.0 GiB is free"
        );
        // Nothing outside, or nothing measured: the owner's models alone.
        for outside in [Some(0), None] {
            let said = in_use(
                &[resident("small", 3)],
                outside,
                "chat/big",
                20 * GIB,
                15 * GIB,
            );
            assert_eq!(
                said,
                "chat/small (3.0 GiB) — chat/big needs 20.0 GiB and 15.0 GiB is free"
            );
        }
        let said = in_use(
            &[resident("small", 3)],
            None,
            "chat/big",
            20 * GIB,
            15 * GIB,
        );
        assert_eq!(
            said,
            "chat/small (3.0 GiB) — chat/big needs 20.0 GiB and 15.0 GiB is free"
        );
        // Nothing of lmgw's on the card: only the outside says why.
        let said = in_use(&[], Some(6 * GIB), "chat/big", 20 * GIB, 15 * GIB);
        assert!(
            said.starts_with("applications outside lmgw (about 6.0 GiB) — "),
            "{said}"
        );
        let said = in_use(&[], None, "chat/big", 20 * GIB, 15 * GIB);
        assert!(said.starts_with("applications outside lmgw — "), "{said}");
    }
}
