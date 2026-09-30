//! A Chat thread's sampling parameters: `top_p`, `top_k`, `min_p`,
//! `repeat_penalty`, the two penalties, `seed` and `stop`, kept with the
//! thread and sent with every turn on top of the alias's own defaults, like
//! `temperature` and `max_tokens`.
//!
//! The Chat never sends a parameter the route cannot take ([`split`]): the
//! thread's own choice that the route refuses is reported with the turn
//! rather than sent to be silently dropped — or worse, rejected — upstream.
//! An API client's request is not filtered; this is Chat-only.

use crate::config::{Protocol, Route, UpstreamKind};
use crate::ir::Params;
use crate::store::ChatThread;

/// The thread's sampling choices as request params; everything else unset.
pub(super) fn params_of(t: &ChatThread) -> Params {
    Params {
        temperature: t.temperature,
        top_p: t.top_p,
        top_k: t.top_k.and_then(|k| u32::try_from(k).ok()),
        min_p: t.min_p,
        repeat_penalty: t.repeat_penalty,
        presence_penalty: t.presence_penalty,
        frequency_penalty: t.frequency_penalty,
        seed: t.seed,
        stop: t.stop.clone(),
        ..Default::default()
    }
}

/// Normalise and check a thread's sampling values before they are stored: a
/// blank stop sequence is no sequence, and every number must be one the
/// samplers accept — refused by name, nothing half-applied.
pub(super) fn check(t: &mut ChatThread) -> Result<(), String> {
    t.stop.retain(|s| !s.is_empty());
    let unit = |name: &str, v: Option<f64>| match v {
        Some(x) if !(0.0..=1.0).contains(&x) => {
            Err(format!("{name}: expected a number from 0 to 1, got {x}"))
        }
        _ => Ok(()),
    };
    unit("top_p", t.top_p)?;
    unit("min_p", t.min_p)?;
    if let Some(k) = t.top_k {
        if !(0..=i64::from(u32::MAX)).contains(&k) {
            return Err(format!(
                "top_k: expected a whole number from 0 to {}, got {k}",
                u32::MAX
            ));
        }
    }
    if let Some(r) = t.repeat_penalty.filter(|r| !(*r > 0.0)) {
        return Err(format!(
            "repeat_penalty: expected a number above 0, got {r}"
        ));
    }
    for (name, v) in [
        ("presence_penalty", t.presence_penalty),
        ("frequency_penalty", t.frequency_penalty),
    ] {
        if let Some(x) = v.filter(|x| !(-2.0..=2.0).contains(x)) {
            return Err(format!("{name}: expected a number from -2 to 2, got {x}"));
        }
    }
    Ok(())
}

/// Split the thread's own params into what `route` takes and the names of
/// what it does not: llama-server takes all of them; a generic
/// OpenAI-compatible provider knows no `top_k`, `min_p` or `repeat_penalty`;
/// Anthropic takes `temperature`, `top_p`, `top_k` and `stop` and nothing
/// else; Gemini takes those and `seed`. Only fields the
/// thread set are reported, in a fixed order; `max_tokens` and `reasoning`
/// are not sampling and pass through untouched.
pub(super) fn split(params: &Params, route: &Route) -> (Params, Vec<&'static str>) {
    split_for(params, route.upstream.protocol, route.upstream.kind)
}

/// [`split`] on the two facts it reads.
fn split_for(
    params: &Params,
    protocol: Protocol,
    kind: UpstreamKind,
) -> (Params, Vec<&'static str>) {
    let (top_k, min_p, repeat, penalties, seed) = match (protocol, kind) {
        (Protocol::Openai, UpstreamKind::LlamaServer) => (true, true, true, true, true),
        (Protocol::Openai, _) => (false, false, false, true, true),
        (Protocol::Anthropic, _) => (true, false, false, false, false),
        (Protocol::Gemini, _) => (true, false, false, false, true),
    };
    let mut sent = params.clone();
    let mut ignored = Vec::new();
    let mut drop = |name: &'static str, set: bool, takes: bool| {
        if set && !takes {
            ignored.push(name);
        }
        set && !takes
    };
    if drop("top_k", sent.top_k.is_some(), top_k) {
        sent.top_k = None;
    }
    if drop("min_p", sent.min_p.is_some(), min_p) {
        sent.min_p = None;
    }
    if drop("repeat_penalty", sent.repeat_penalty.is_some(), repeat) {
        sent.repeat_penalty = None;
    }
    if drop(
        "presence_penalty",
        sent.presence_penalty.is_some(),
        penalties,
    ) {
        sent.presence_penalty = None;
    }
    if drop(
        "frequency_penalty",
        sent.frequency_penalty.is_some(),
        penalties,
    ) {
        sent.frequency_penalty = None;
    }
    if drop("seed", sent.seed.is_some(), seed) {
        sent.seed = None;
    }
    (sent, ignored)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all() -> Params {
        Params {
            temperature: Some(0.5),
            top_p: Some(0.9),
            top_k: Some(40),
            min_p: Some(0.05),
            repeat_penalty: Some(1.1),
            presence_penalty: Some(0.1),
            frequency_penalty: Some(0.2),
            seed: Some(7),
            stop: vec!["END".into()],
            ..Default::default()
        }
    }

    #[test]
    fn llama_server_takes_everything() {
        let (sent, ignored) = split_for(&all(), Protocol::Openai, UpstreamKind::LlamaServer);
        assert_eq!(sent, all());
        assert!(ignored.is_empty());
    }

    #[test]
    fn generic_openai_drops_the_llama_extensions_only() {
        let (sent, ignored) = split_for(&all(), Protocol::Openai, UpstreamKind::Generic);
        assert_eq!(ignored, ["top_k", "min_p", "repeat_penalty"]);
        assert_eq!(sent.top_k, None);
        assert_eq!(sent.min_p, None);
        assert_eq!(sent.repeat_penalty, None);
        assert_eq!(sent.presence_penalty, Some(0.1));
        assert_eq!(sent.seed, Some(7));
        assert_eq!(sent.stop, ["END"]);
    }

    #[test]
    fn anthropic_takes_top_k_and_stop_but_no_penalties_or_seed() {
        let (sent, ignored) = split_for(&all(), Protocol::Anthropic, UpstreamKind::Generic);
        assert_eq!(
            ignored,
            [
                "min_p",
                "repeat_penalty",
                "presence_penalty",
                "frequency_penalty",
                "seed"
            ]
        );
        assert_eq!(sent.top_k, Some(40));
        assert_eq!(sent.top_p, Some(0.9));
        assert_eq!(sent.stop, ["END"]);
    }

    #[test]
    fn gemini_takes_seed_and_top_k() {
        let (sent, ignored) = split_for(&all(), Protocol::Gemini, UpstreamKind::Generic);
        assert_eq!(
            ignored,
            [
                "min_p",
                "repeat_penalty",
                "presence_penalty",
                "frequency_penalty"
            ]
        );
        assert_eq!(sent.seed, Some(7));
        assert_eq!(sent.top_k, Some(40));
    }

    #[test]
    fn unset_params_are_never_reported() {
        let (_, ignored) = split_for(
            &Params::default(),
            Protocol::Anthropic,
            UpstreamKind::Generic,
        );
        assert!(ignored.is_empty());
    }

    fn thread() -> ChatThread {
        ChatThread::default()
    }

    #[test]
    fn check_refuses_out_of_range_values_by_name() {
        let cases: [(fn(&mut ChatThread), &str); 6] = [
            (|t| t.top_p = Some(1.5), "top_p"),
            (|t| t.min_p = Some(-0.1), "min_p"),
            (|t| t.top_k = Some(-1), "top_k"),
            (|t| t.repeat_penalty = Some(0.0), "repeat_penalty"),
            (|t| t.presence_penalty = Some(2.5), "presence_penalty"),
            (|t| t.frequency_penalty = Some(-3.0), "frequency_penalty"),
        ];
        for (set, name) in cases {
            let mut t = thread();
            set(&mut t);
            let err = check(&mut t).unwrap_err();
            assert!(err.starts_with(name), "{err}");
        }
    }

    #[test]
    fn check_accepts_the_edges_and_drops_blank_stops() {
        let mut t = thread();
        t.top_p = Some(1.0);
        t.min_p = Some(0.0);
        t.top_k = Some(0);
        t.presence_penalty = Some(-2.0);
        t.frequency_penalty = Some(2.0);
        t.stop = vec!["a".into(), String::new()];
        assert!(check(&mut t).is_ok());
        assert_eq!(t.stop, ["a"]);
    }
}
