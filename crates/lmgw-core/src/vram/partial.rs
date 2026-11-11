//! A start that fits only partially on the card (the owner's ruling of 2026-11-11: a start that would work is never blocked; it goes ahead with a warning that it fits only partially on the GPU and may run slower).
//!
//! **Who.** An image pipeline that can run with less than all of it on the
//! card: `offload_to_cpu`, a `params_backend` naming `cpu` or `disk`, or
//! sd.cpp's auto-fit, on unless the row turns it off ([`super::plan::image`]).
//! Such a row is never too large for a card. One that pins its weights there
//! (`auto_fit off`, a `params_backend` of GPU devices only) is refused when it
//! is larger than the card, and the refusal names what would let it run with
//! less ([`too_large_hint`]). A llama-server row llama.cpp's `--fit` would
//! shrink is fitted by [`llama`] instead; an aux row's partial
//! `--n-gpu-layers` and `--fit` are not counted as partial yet, and admission
//! charges them in full.
//!
//! **What it is given** ([`decide`]), in this order — less than the whole
//! only where its engine sees exactly one card ([`cards_seen`]; partial
//! fitting on several cards is not done yet, see below), and for a row whose
//! args set their own `max_vram` only that budget and its context, which
//! wins at render (one of 0 or less bounds nothing and needs the whole):
//! 1. The whole — its files, as every pipeline is charged — wherever that
//!    fits in what is free beyond the headroom, after the evictions any start
//!    makes. No warning, and sd-server sizes itself as it always did.
//! 2. Else its on-card part ([`PartialFit::on_card_bytes`]) where that fits.
//! 3. Else what is free beyond the headroom, when the budget that leaves —
//!    that less its context — is at least the smallest budget a generation
//!    of the row has completed under ([`PartialFit::work_bytes`], learned by
//!    [`work`]). Its measured context ([`floor`]) is no working budget: on
//!    a 4 GiB Windows test card sd-server's Vulkan context is 5 MiB, and Z-Image-Turbo
//!    fails at `--max-vram 0.5` and renders at 1 (2026-11-11).
//! 4. Else — no working budget learned yet, or less than it — the start goes
//!    ahead with what is free only when waiting could not make more room —
//!    nothing else of lmgw's on the card or reserved for it — and no
//!    fallback would answer in its place. Otherwise it waits, drains or
//!    evicts, and a configured fallback answers, as for any model that does
//!    not fit. A budget of nothing at all beside a measured context never
//!    starts. Its warning says which of these it is.
//!
//! **Held to it.** sd.cpp's auto-fit and its weight streaming size themselves
//! to the driver's live free memory, which includes the headroom, the peaks
//! the ledger keeps free for other pipelines, audio's pending loads and the
//! weights of a model still loading. So a partial start is held to what it
//! was given: `--max-vram`, "optional per-device budget in GiB for managed
//! weights and runner buffers" (sd-server's help). sd.cpp parses it as a
//! float (`parse_strict_float`, `master-1079-a3c49b1` and the spike's newer
//! tree alike), and its model manager checks every runner's request against
//! it whatever placed the weights, so it binds `offload_to_cpu` and an
//! explicit `params_backend` as much as auto-fit: measured on a GTX 970 test card (Vulkan), `offload_to_cpu` drew 3.0 GiB in one 512² generation with no
//! budget, 1010 MiB with `--max-vram 1` and 2038 MiB with 2. It is rendered
//! as the absolute budget — a negative one would again follow the live free
//! figure, and 0 means live free — and is the grant less the context: the
//! measured one, else [`CONTEXT_STAND_IN`] set aside where the grant leaves
//! room for it. A `max_vram` in the row's args wins. The grant is staged for
//! the registry entry (`registry::admission_grant`), which renders it into
//! every launch, records it on the container (`lmgw.grant`, so boot adopts
//! it), keeps the warning for the entry's life and is what the ledger
//! charges the entry — its grant, or its budget and its context where that
//! is more.
//!
//! **Learned under an engine.** The context, the working budget and the
//! peak count only under the engine key the row resolves to now (a running
//! container's: the one it started with), [`super::plan::image::engine_key`]:
//! figures learned under another engine image, run args or native build are
//! not measured, and the rules for unmeasured figures apply.
//!
//! **Not yet.** The grant is the entry's for its whole life: a pipeline
//! squeezed once stays squeezed, and warned, after the card frees up, until
//! its container stops. `--max-vram` is per device while the ledger pools
//! devices, so a row whose engine sees several cards takes the
//! whole-pipeline path; per-device budgets are not done yet. The working
//! budget is the row's, not per size ([`work`]).
//!
//! **Said.** Where the whole does not fit, the start logs the warning and
//! its registry entry carries it (the Models page, the start and test
//! answers); [`on_card`]'s sentence is what `local_model_get` reads.

use std::sync::atomic::Ordering;

use crate::config::Snapshot;
use crate::hf::fmt_bytes;
use crate::runtime::registry::PartialGrant;
use crate::runtime::Class;

use super::plan::image::{placement, PartialFit, CONTEXT_STAND_IN};
use super::{Footprint, Target};

pub(super) mod floor;
pub mod llama;
pub mod work;

/// The warning's lead.
pub const PARTIAL_FIT: &str = "It will work but only fits partially on the GPU, expect slowdowns";

/// What admission gives a start it lets go ahead.
pub(super) enum Grant {
    /// The whole footprint, in bytes: the start of any model.
    Whole(u64),
    /// The whole footprint, with a warning the entry keeps: the learned
    /// growth of other programs' use it goes ahead without
    /// (`VramScheduler::growth_let_go`).
    Noted(u64, String),
    /// Less than the whole, with the budget and the warning.
    Partial(PartialGrant),
}

/// What a decision knows besides the footprint.
#[derive(Debug, Clone, Copy)]
pub(super) struct Room {
    /// The ledger's free figure.
    pub free: u64,
    pub headroom: u64,
    /// Something else of lmgw's is on the card or reserved for it, which a
    /// wait could free.
    pub others_hold: bool,
    /// A fallback would answer in this model's place.
    pub fallback: bool,
    /// How many GPUs the model's engine would see ([`cards_seen`]); `None`
    /// where that is not known. A partial grant needs exactly one: sd.cpp's
    /// `--max-vram` is a per-device budget and the ledger pools the cards.
    pub cards: Option<usize>,
}

impl Room {
    fn beyond_headroom(&self) -> u64 {
        self.free.saturating_sub(self.headroom)
    }
}

/// Whether `fp` is a model that can run with less than all of it on the card
/// (module doc): never too large for a card.
pub(super) fn adapts(fp: &Footprint) -> bool {
    fp.partial.is_some() || llama::adapts(fp)
}

/// The least that must be free beyond the headroom for `fp` to go ahead
/// without waiting for anything (module doc, steps 3 and 4): its context and
/// the working budget it has rendered with, else its on-card part. §4.7's
/// outside-VRAM verdict measures against it.
///
/// The row's own `max_vram` is its budget whatever admission would give
/// (`own_budget`): its least is that budget and its context, and one that
/// follows the driver's free memory (0 or less) needs the whole. So does a
/// row on other than exactly one card (`cards`, [`Room::cards`]).
pub(super) fn least(fp: &Footprint, cards: Option<usize>) -> u64 {
    // A llama-server row fitted to the card: its least placement. Its
    // target is fitted to what the card can give it at admission — the
    // card less what other programs hold now, lmgw's own models counted as
    // room — so wherever its least fits beside other programs' memory now,
    // a target does, and admission waits for or evicts lmgw's own models
    // for it (`llama`); only less than that is a shortfall the fallback
    // answers.
    if fp.llama.is_some() {
        return llama::least_bytes(fp, cards);
    }
    match &fp.partial {
        _ if cards != Some(1) => fp.total_bytes,
        Some(p) => match (p.own_budget, p.work_bytes) {
            (Some(Some(b)), _) => p.context().saturating_add(b),
            (Some(None), _) => fp.total_bytes,
            (None, Some(w)) => p.context().saturating_add(w).min(p.on_card_bytes),
            (None, None) => p.on_card_bytes,
        }
        .min(fp.total_bytes),
        None => fp.total_bytes,
    }
}

/// What `target` with footprint `fp` is given in `room` (module doc), or
/// `None` when it must take the normal path — wait, evict, fall back, or be
/// refused.
pub(super) fn decide(target: &Target, fp: &Footprint, room: Room) -> Option<Grant> {
    if room.free >= fp.total_bytes.saturating_add(room.headroom) {
        return Some(whole(target, fp, room));
    }
    if fp.llama.is_some() {
        return llama::decide(target, fp, room);
    }
    let p = fp.partial.as_ref()?;
    // `--max-vram` is per device while the ledger pools them: on several
    // cards sd-server could draw its budget on each (not done yet).
    if room.cards != Some(1) {
        return None;
    }
    let avail = room.beyond_headroom();
    if avail == 0 {
        return None;
    }
    let context = p.context();
    // The row's own `max_vram` wins at render: a partial grant is that
    // budget and its context, or nothing — and one that follows the
    // driver's free memory is not bounded at all.
    match p.own_budget {
        Some(Some(b)) => {
            let bytes = b.saturating_add(context);
            if avail < bytes {
                return None;
            }
            return Some(Grant::Partial(PartialGrant {
                bytes,
                max_vram_bytes: b,
                warning: warning(target, fp, p, room, (bytes, b), Squeezed::Own),
                llama: None,
            }));
        }
        Some(None) => return None,
        None => {}
    }
    let (bytes, squeezed) = if p.on_card_bytes > 0 && avail >= p.on_card_bytes {
        (p.on_card_bytes, Squeezed::No)
    } else {
        let budget = avail.saturating_sub(context);
        match p.work_bytes {
            Some(w) if budget >= w => (avail, Squeezed::Known(w)),
            // A measured context that takes all of it leaves no budget.
            _ if budget == 0 && p.context_bytes.is_some() => return None,
            _ if !room.others_hold && !room.fallback => (avail, Squeezed::Unmeasured),
            _ => return None,
        }
    };
    // The budget is the grant less the context; where the stand-in would
    // leave nothing, the whole grant, and the warning says so.
    let max_vram_bytes = match bytes.saturating_sub(context) {
        0 => bytes,
        b => b,
    };
    Some(Grant::Partial(PartialGrant {
        bytes,
        max_vram_bytes,
        warning: warning(target, fp, p, room, (bytes, max_vram_bytes), squeezed),
        llama: None,
    }))
}

/// The grant of a start that got its whole charge: [`Grant::Whole`], or for
/// a row llama.cpp's `--fit` would shrink the placement it was fitted to,
/// which the start renders in `--fit`'s place ([`llama::whole`]).
pub(super) fn whole(target: &Target, fp: &Footprint, room: Room) -> Grant {
    llama::whole(target, fp, room).unwrap_or(Grant::Whole(fp.total_bytes))
}

/// How a partial grant came to be less than the on-card part.
#[derive(Clone, Copy)]
enum Squeezed {
    No,
    /// The row's own `max_vram` and its context.
    Own,
    /// At least the working budget it has rendered with.
    Known(u64),
    /// No working budget learned, or less than it: nothing to wait for.
    Unmeasured,
}

fn warning(
    target: &Target,
    fp: &Footprint,
    p: &PartialFit,
    room: Room,
    (bytes, max_vram): (u64, u64),
    squeezed: Squeezed,
) -> String {
    let short = || {
        format!(
            "{}, all that is free beyond the headroom: short of the {} it would take ({})",
            fmt_bytes(bytes),
            fmt_bytes(p.on_card_bytes),
            p.basis
        )
    };
    let given = match squeezed {
        Squeezed::No => format!("{} on the card ({})", fmt_bytes(bytes), p.basis),
        Squeezed::Own => format!(
            "{} on the card, the budget the max_vram in its args sets and its context",
            fmt_bytes(bytes)
        ),
        Squeezed::Known(w) => format!(
            "{}, a budget of at least the {} it has rendered with",
            short(),
            fmt_bytes(w)
        ),
        Squeezed::Unmeasured => format!(
            "{}, with nothing else of lmgw's on the card to wait for and no fallback to answer; \
             {}",
            short(),
            match p.work_bytes {
                Some(w) => format!("less than the {} budget it has rendered with", fmt_bytes(w)),
                None => "how little it can run with is not measured yet".into(),
            }
        ),
    };
    let context = match p.context_bytes {
        Some(c) => format!("its measured {} context outside that budget", fmt_bytes(c)),
        None if max_vram < bytes => format!(
            "{} set aside for its context outside that budget, not measured yet",
            fmt_bytes(CONTEXT_STAND_IN)
        ),
        None => "its context, not measured yet, on top: the grant left no room to set aside \
                 for it"
            .into(),
    };
    let held = match squeezed {
        Squeezed::Own => format!(
            "its own max_vram holds sd-server to {} GiB (--max-vram)",
            crate::runtime::argv::max_vram_arg(max_vram)
        ),
        _ => format!(
            "holds sd-server to {} GiB (--max-vram)",
            crate::runtime::argv::max_vram_arg(max_vram)
        ),
    };
    format!(
        "{PARTIAL_FIT}: {}/{} needs {} in full plus {} headroom and {} is free; {} {}. VRAM \
         admission gives it {given}, and {held}, {context}",
        target.class.as_str(),
        target.model_id,
        fmt_bytes(fp.total_bytes),
        fmt_bytes(room.headroom),
        fmt_bytes(room.free),
        p.how.setting(),
        p.how.does(),
    )
}

/// Why `fp` is not given anything in `room`, for a refusal or a wait's log
/// line: `None` for a model all on the card, whose figures say it already.
pub(super) fn why_not(fp: &Footprint, room: Room) -> Option<String> {
    if fp.llama.is_some() {
        return llama::why_not(fp, room);
    }
    let p = fp.partial.as_ref()?;
    let avail = room.beyond_headroom();
    let who = if room.fallback {
        "a fallback answers in its place"
    } else {
        "lmgw's other models hold the card"
    };
    let setting = p.how.setting();
    Some(match (p.context_bytes, p.work_bytes) {
        _ if room.cards != Some(1) => format!(
            "{setting} could run with less of it on the card, but {}: sd.cpp's --max-vram is a \
             per-device budget and VRAM admission pools the cards, so fitting it partially on \
             several cards is not done yet and it needs all of it free",
            match room.cards {
                Some(0) => "its run args give its engine no card".to_string(),
                Some(n) => format!("its engine would see {n} cards"),
                None => "how many cards its engine would see is not known (no device reading)"
                    .to_string(),
            }
        ),
        _ if p.own_budget == Some(None) => format!(
            "{setting} could run with less of it on the card, but the max_vram in its args (0 or \
             less) lets sd-server follow the driver's free memory, which leaves it unbounded, so \
             it needs all of it free"
        ),
        _ if avail == 0 => format!(
            "{setting} could run with less of it on the card, but nothing is free beyond the \
             headroom"
        ),
        _ if matches!(p.own_budget, Some(Some(_))) => format!(
            "{setting} could run with less of it on the card, but the max_vram in its args holds \
             sd-server to {}, which with its context ({}) is more than the {} free beyond the \
             headroom",
            fmt_bytes(p.own_budget.flatten().unwrap_or(0)),
            fmt_bytes(p.context()),
            fmt_bytes(avail)
        ),
        (Some(c), _) if avail <= c => format!(
            "{setting} could run with less of it on the card, but the {} free beyond the \
             headroom is no more than its measured context, {}, which leaves it no budget",
            fmt_bytes(avail),
            fmt_bytes(c)
        ),
        (_, Some(w)) => format!(
            "{setting} could run with less of it on the card, but the {} free beyond the \
             headroom leaves a budget of {}, less than the {} it has rendered with, so it does \
             not start on that while {who}",
            fmt_bytes(avail),
            fmt_bytes(avail.saturating_sub(p.context())),
            fmt_bytes(w)
        ),
        (_, None) => format!(
            "{setting} could run with less of it on the card, but how little is not measured \
             yet, so it does not start on less than its {} ({}) while {who}",
            fmt_bytes(p.on_card_bytes),
            p.basis
        ),
    })
}

/// What a `max_vram` in the row's own `args` holds sd-server to, which wins
/// over a grant: `Some(Some(bytes))` for a budget (the largest of a
/// per-device assignment), `Some(None)` for none at all (0 or a negative
/// value follows the driver's live free memory), `None` where the row sets
/// none. sd.cpp's units: GiB of 1024³.
pub(crate) fn own_budget(args: &serde_json::Map<String, serde_json::Value>) -> Option<Option<u64>> {
    let v = args
        .iter()
        .find(|(k, _)| crate::sdcpp_caps::canonical_key(k) == "max_vram")?
        .1;
    let text = match v {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    let mut most: Option<f64> = None;
    for part in text.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        let value = part.rsplit_once('=').map_or(part, |(_, v)| v.trim());
        match value.parse::<f64>() {
            Ok(g) if g.is_finite() && g > 0.0 => most = Some(most.map_or(g, |m| m.max(g))),
            _ => return Some(None),
        }
    }
    Some(most.map(|g| (g * (1024.0 * 1024.0 * 1024.0)) as u64))
}

/// The same warning for the row, against the card's `total` rather than what
/// is free now: `None` unless the whole of it, with the headroom, is more than
/// the card holds. `cards`: [`Room::cards`].
pub fn on_card(
    target: &Target,
    fp: &Footprint,
    total: u64,
    headroom: u64,
    cards: Option<usize>,
) -> Option<String> {
    let p = fp.partial.as_ref()?;
    if fp.total_bytes.saturating_add(headroom) <= total {
        return None;
    }
    let lead = format!(
        "{PARTIAL_FIT}: {}/{} needs {} in full plus {} headroom and the GPU holds {}; {} {}",
        target.class.as_str(),
        target.model_id,
        fmt_bytes(fp.total_bytes),
        fmt_bytes(headroom),
        fmt_bytes(total),
        p.how.setting(),
        p.how.does(),
    );
    let room = Room {
        free: total,
        headroom,
        others_hold: false,
        fallback: false,
        cards,
    };
    // Where admission gives it nothing partial, the reason why_not gives.
    if cards != Some(1) || p.own_budget == Some(None) {
        return Some(format!("{lead}. {}", why_not(fp, room).unwrap_or_default()));
    }
    Some(match p.own_budget {
        Some(Some(b)) => format!(
            "{lead}. VRAM admission gives it the {} budget the max_vram in its args sets and its \
             context ({}) on the card once that much is free",
            fmt_bytes(b),
            fmt_bytes(p.context())
        ),
        _ => format!(
            "{lead}. VRAM admission gives it {} on the card ({}) or what is free, and holds \
             sd-server to that (--max-vram)",
            fmt_bytes(p.on_card_bytes),
            p.basis
        ),
    })
}

/// How many GPUs image row `target`'s engine would see, with `devices` the
/// devices the last ledger read listed and `build` the engine build the
/// launcher would start ([`super::VramScheduler::engine_build`]): the cards
/// its resolved run args name, else every device; none where they pass no
/// card. A native Vulkan build sees every card whatever the run args name
/// (the card selection does not reach it yet, the native start's warning).
/// `None` where it is not known: no row, or no device reading for "every
/// card".
pub(super) fn cards_seen(
    snap: &Snapshot,
    target: &Target,
    devices: usize,
    build: Option<&str>,
) -> Option<usize> {
    use crate::runtime::launcher::native::gpu::{selection, Selection};
    let args = match target.class {
        Class::Image => snap
            .image_models
            .iter()
            .find(|m| m.model_id == target.model_id)?
            .extra_run_args
            .as_ref()
            .unwrap_or(&snap.settings.image.extra_run_args),
        Class::Chat => snap
            .local_models
            .iter()
            .find(|m| m.model_id == target.model_id)?
            .extra_run_args
            .as_ref()
            .unwrap_or(&snap.settings.router.extra_run_args),
        _ => return None,
    };
    let every = (devices > 0).then_some(devices);
    match selection(args, crate::runtime::Placement::Gpu) {
        Selection::None => Some(0),
        Selection::All => every,
        Selection::Nvidia(_) if build.is_some_and(|b| b.ends_with("-vulkan")) => every,
        Selection::Nvidia(names) => {
            let mut names: Vec<&str> = names.iter().map(|n| n.trim()).collect();
            names.sort_unstable();
            names.dedup();
            Some(names.len())
        }
    }
}

/// The end of a refusal for a model too large for the card: for an image
/// pipeline that pins its weights on the card, and so needs all of it there,
/// the setting that pins it and what would let it run with less. Empty for
/// everything else.
pub(super) fn too_large_hint(snap: &Snapshot, target: &Target, fp: &Footprint) -> String {
    if fp.llama.is_some() {
        return llama::too_large_hint(fp);
    }
    if target.class != Class::Image {
        return String::new();
    }
    let Some(row) = snap
        .image_models
        .iter()
        .find(|m| m.model_id == target.model_id)
    else {
        return String::new();
    };
    match placement(&row.args) {
        Err(pinned) => format!(
            ". {} keeps all of its weights on the card; offload_to_cpu, a params_backend of cpu \
             or disk, or sd.cpp's auto-fit (no auto_fit off and no params_backend) in its args \
             would let it run with less",
            pinned.setting()
        ),
        Ok(_) => String::new(),
    }
}

impl super::VramScheduler {
    /// [`on_card`] for `class`/`model_id` as configured, against the card's
    /// total: a declared budget, else the devices' total as the last ledger
    /// read measured it, else one device read now. `None` when the row pins
    /// its weights on the card, fits it, or there is no card to say it of.
    pub async fn partial_fit_note(
        &self,
        snap: &Snapshot,
        class: Class,
        model_id: &str,
    ) -> Option<String> {
        let fp = self.footprint(snap, class, model_id).await?;
        fp.partial.as_ref()?;
        let (total, devices) = match self.known_total(snap) {
            Some(t) => (t, self.device_count.load(Ordering::Relaxed)),
            None => {
                let d = self.devices().await.ok()?;
                (super::ledger::pooled(0, &d).0, d.len())
            }
        };
        if total == 0 {
            return None;
        }
        let target = Target {
            class,
            model_id: model_id.to_string(),
        };
        let cards = cards_seen(snap, &target, devices, self.engine_build(class).as_deref());
        let headroom = self.start_headroom(snap, class, model_id, &fp);
        on_card(&target, &fp, total, headroom, cards)
    }

    /// [`cards_seen`] against the devices the last ledger read listed.
    pub(super) fn cards_for(&self, snap: &Snapshot, target: &Target) -> Option<usize> {
        cards_seen(
            snap,
            target,
            self.device_count.load(Ordering::Relaxed),
            self.engine_build(target.class).as_deref(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::super::plan::image::Adapts;
    use super::super::scheduler::MIB;
    use super::*;

    const GIB: u64 = 1024 * MIB;

    fn fp(total: u64, on_card: u64, context: Option<u64>, work: Option<u64>) -> Footprint {
        let mut fp = Footprint::new(total, 0, None, None);
        fp.partial = Some(PartialFit {
            how: Adapts::ToCpu,
            on_card_bytes: on_card,
            basis: "its largest file, diffusion_model, and its context".into(),
            context_bytes: context,
            work_bytes: work,
            own_budget: None,
        });
        fp
    }

    fn target() -> Target {
        Target {
            class: Class::Image,
            model_id: "turbo".into(),
        }
    }

    fn room(free: u64, others_hold: bool, fallback: bool) -> Room {
        Room {
            free,
            headroom: GIB / 2,
            others_hold,
            fallback,
            cards: Some(1),
        }
    }

    fn partial(g: Option<Grant>) -> PartialGrant {
        match g {
            Some(Grant::Partial(p)) => p,
            Some(Grant::Whole(b) | Grant::Noted(b, _)) => panic!("the whole, {b}"),
            None => panic!("nothing"),
        }
    }

    #[test]
    fn the_whole_wherever_it_fits() {
        let f = fp(6 * GIB, 3 * GIB, None, None);
        assert!(matches!(
            decide(&target(), &f, room(7 * GIB, true, true)),
            Some(Grant::Whole(b)) if b == 6 * GIB
        ));
    }

    #[test]
    fn then_the_on_card_part_held_with_max_vram() {
        let f = fp(6 * GIB, 3 * GIB, Some(GIB), None);
        let g = partial(decide(&target(), &f, room(5 * GIB, true, true)));
        assert_eq!(g.bytes, 3 * GIB);
        assert_eq!(g.max_vram_bytes, 2 * GIB, "less the measured context");
        assert!(g.warning.starts_with(PARTIAL_FIT), "{}", g.warning);
        assert!(
            g.warning
                .contains("gives it 3.0 GiB on the card (its largest file")
                && g.warning
                    .contains("holds sd-server to 2.000 GiB (--max-vram)")
                && g.warning
                    .contains("its measured 1.0 GiB context outside that budget"),
            "{}",
            g.warning
        );
    }

    /// With no context measured, the stand-in
    /// comes out of the grant, so what admission reserves is what sd-server
    /// can draw.
    #[test]
    fn an_unmeasured_context_is_set_aside_inside_the_grant() {
        let f = fp(6 * GIB, 3 * GIB + CONTEXT_STAND_IN, None, None);
        let g = partial(decide(&target(), &f, room(5 * GIB, true, true)));
        assert_eq!(g.bytes, 4 * GIB);
        assert_eq!(g.max_vram_bytes, 3 * GIB);
        assert!(
            g.warning
                .contains("1.0 GiB set aside for its context outside that budget, not measured"),
            "{}",
            g.warning
        );
        // A grant the stand-in would empty holds sd-server to all of it, and
        // says the context comes on top.
        let f = fp(6 * GIB, 3 * GIB + CONTEXT_STAND_IN, None, None);
        let g = partial(decide(&target(), &f, room(GIB + GIB / 4, false, false)));
        assert_eq!((g.bytes, g.max_vram_bytes), (3 * GIB / 4, 3 * GIB / 4));
        assert!(
            g.warning.contains("left no room to set aside"),
            "{}",
            g.warning
        );
    }

    /// The measured context is no working budget. Just above it,
    /// with others on the card or a fallback, the start takes the normal
    /// path — no 0.001 GiB budget.
    #[test]
    fn just_above_the_context_is_no_working_budget() {
        let f = fp(6 * GIB, 3 * GIB, Some(5 * MIB), None);
        // 1.2 GiB beyond the headroom, an idle chat model, a cloud fallback.
        let r = room(GIB / 2 + 6 * GIB / 5, true, true);
        assert!(decide(&target(), &f, r).is_none());
        let why = why_not(&f, r).unwrap();
        assert!(
            why.contains("how little is not measured yet")
                && why.contains("a fallback answers in its place"),
            "{why}"
        );
        // Nothing to wait for and no fallback: it goes ahead, saying so.
        let g = partial(decide(
            &target(),
            &f,
            room(GIB / 2 + 6 * GIB / 5, false, false),
        ));
        assert_eq!(g.max_vram_bytes, 6 * GIB / 5 - 5 * MIB);
        assert!(
            g.warning.contains("no fallback to answer") && g.warning.contains("not measured yet"),
            "{}",
            g.warning
        );
        // A measured context with nothing above it: never.
        let f = fp(6 * GIB, 3 * GIB, Some(GIB), None);
        let r = room(GIB / 2 + GIB, false, false);
        assert!(decide(&target(), &f, r).is_none());
        assert!(why_not(&f, r)
            .unwrap()
            .contains("which leaves it no budget"));
    }

    #[test]
    fn a_learned_working_budget_decides_beside_others() {
        let f = fp(6 * GIB, 4 * GIB, Some(GIB / 4), Some(GIB));
        // 1.5 GiB beyond the headroom: a 1.25 GiB budget, above the 1 GiB it
        // has rendered with, goes ahead beside others and a fallback.
        let g = partial(decide(&target(), &f, room(2 * GIB, true, true)));
        assert_eq!((g.bytes, g.max_vram_bytes), (3 * GIB / 2, 5 * GIB / 4));
        assert!(
            g.warning
                .contains("a budget of at least the 1.0 GiB it has rendered with"),
            "{}",
            g.warning
        );
        // 1 GiB beyond: a 768 MiB budget, below it: the Unmeasured rule.
        let r = room(3 * GIB / 2, true, false);
        assert!(decide(&target(), &f, r).is_none());
        let why = why_not(&f, r).unwrap();
        assert!(
            why.contains("leaves a budget of 768 MiB, less than the 1.0 GiB it has rendered with")
                && why.contains("lmgw's other models hold the card"),
            "{why}"
        );
        let g = partial(decide(&target(), &f, room(3 * GIB / 2, false, false)));
        assert!(
            g.warning
                .contains("less than the 1.0 GiB budget it has rendered with"),
            "{}",
            g.warning
        );
    }

    #[test]
    fn least_is_context_and_working_budget_else_the_on_card_part() {
        let one = Some(1);
        assert_eq!(
            least(&fp(6 * GIB, 3 * GIB, Some(GIB), Some(GIB)), one),
            2 * GIB
        );
        assert_eq!(
            least(&fp(6 * GIB, 3 * GIB, None, Some(GIB)), one),
            GIB + CONTEXT_STAND_IN
        );
        assert_eq!(least(&fp(6 * GIB, 3 * GIB, Some(GIB), None), one), 3 * GIB);
        assert_eq!(least(&Footprint::new(6 * GIB, 0, None, None), one), 6 * GIB);
        // On several cards, or a count not known: the whole.
        assert_eq!(
            least(&fp(6 * GIB, 3 * GIB, Some(GIB), Some(GIB)), Some(2)),
            6 * GIB
        );
        assert_eq!(least(&fp(6 * GIB, 3 * GIB, Some(GIB), None), None), 6 * GIB);
    }
}
