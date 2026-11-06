//! What [`super::find`] found, as `model_find`'s answer (find-a-model
//! design §4.5, §5, §9.2, D19, D20, D22, D23, D120, D123, D124, D125).
//!
//! The card's marks are by size (D8's, through [`fit::by_size`]): the page
//! asks `hf_repo` with `fit=true` for the header's (D123). Both measures are
//! taken (D120): the card shows the file that fits beside what runs now,
//! and the whole card's pick when it differs.

use lmgw_api_types::model_find::{share_name, share_text, share_url};
use lmgw_api_types::model_find::{
    CardFile, CardKind, FileFit, FindAnswer, FitMeasure, HubBudget, ModelCard, ModelChoice,
    ModelFind, ModelFindArgs, ShareLink, UploadChoice, DEFAULT_LIST_COUNT,
};
use lmgw_api_types::{HfCompanion, ModelLicence};

use crate::hf::rate::Engine;
use crate::hf::search::HubModel;
use crate::hf::{self, fmt_bytes};
use crate::modelinfo::fit::{self, CardNow};
use crate::modelinfo::pick::FLOOR_BITS;

use super::query::HubPage;
use super::rank::{Kind, Original};
use super::walk::{self, Chosen, Room, WeightsFile};
use super::{Found, Query};

/// The digits hint every answer that found nothing carries (D18): names are
/// searched as typed, and this says how they are usually written.
pub const DIGITS_HINT: &str =
    "Names are usually written as on the model's page, with numbers as digits.";

/// "27 billion", "600 million", "1.2 trillion".
pub fn parameters_words(n: u64) -> String {
    let (v, unit) = match n {
        n if n >= 1_000_000_000_000 => (n as f64 / 1e12, "trillion"),
        n if n >= 1_000_000_000 => (n as f64 / 1e9, "billion"),
        n if n >= 1_000_000 => (n as f64 / 1e6, "million"),
        n => return n.to_string(),
    };
    let s = format!("{v:.1}");
    format!("{} {unit}", s.strip_suffix(".0").unwrap_or(&s))
}

/// A repo's name as the card says it: hyphens as spaces (D22).
pub fn display(id: &str) -> String {
    lmgw_api_types::model_find::display_name(id)
}

/// "about 2 minutes", "40 seconds".
pub fn duration_words(secs: u64) -> String {
    match secs {
        0..=59 => format!("{secs} seconds"),
        s => match s.div_ceil(60) {
            1 => "about a minute".into(),
            m => format!("about {m} minutes"),
        },
    }
}

/// Which part of the answer's two lists it carries (the WP4 call D142).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Lists {
    pub others_offset: u32,
    pub others_count: u32,
    pub did_you_mean_offset: u32,
    pub did_you_mean_count: u32,
    pub other_uploads_offset: u32,
    pub other_uploads_count: u32,
}

impl Default for Lists {
    fn default() -> Self {
        Self {
            others_offset: 0,
            others_count: DEFAULT_LIST_COUNT,
            did_you_mean_offset: 0,
            did_you_mean_count: DEFAULT_LIST_COUNT,
            other_uploads_offset: 0,
            other_uploads_count: DEFAULT_LIST_COUNT,
        }
    }
}

impl Lists {
    pub fn of(args: &ModelFindArgs) -> Self {
        Self {
            others_offset: args.others_offset,
            others_count: args.others_count,
            did_you_mean_offset: args.did_you_mean_offset,
            did_you_mean_count: args.did_you_mean_count,
            other_uploads_offset: args.other_uploads_offset,
            other_uploads_count: args.other_uploads_count,
        }
    }
}

/// `count` entries of `all` from `offset`, the total, and how many come
/// after them.
fn window<T>(all: Vec<T>, offset: u32, count: u32) -> (Vec<T>, u32, u32) {
    let total = all.len();
    let from = (offset as usize).min(total);
    let to = from.saturating_add(count as usize).min(total);
    let part = all.into_iter().skip(from).take(to - from).collect();
    (part, total as u32, (total - to) as u32)
}

/// What the answer is measured with, read once from this install.
#[derive(Debug, Clone)]
pub struct Inputs {
    /// The card now; `None` without one (D9).
    pub card: Option<CardNow>,
    /// This machine's download rate (D23, D58).
    pub downloads_rate: Option<f64>,
    /// The engine image the card's kind runs in.
    pub engine: Engine,
    /// Free bytes in the card's models directory (D125).
    pub disk_free: Option<u64>,
    /// A Hugging Face token is configured.
    pub token_set: bool,
}

impl Inputs {
    /// The whole card, as the walk measures against it.
    pub fn room(card: Option<&CardNow>) -> Option<Room> {
        card.map(|c| Room {
            capacity: c.room(FitMeasure::WholeCard),
            headroom: c.card.headroom,
        })
    }
}

fn kind_dto(k: &Kind) -> CardKind {
    match k {
        Kind::Embed => CardKind::Embed,
        Kind::Rerank => CardKind::Rerank,
        _ => CardKind::Chat,
    }
}

fn choice(m: &HubModel, matched: u32, note: Option<String>) -> ModelChoice {
    ModelChoice {
        origin: m.id.clone(),
        name: display(&m.id),
        author: m.author().to_string(),
        likes: m.likes,
        downloads: m.downloads,
        pipeline: m.pipeline_tag.clone(),
        parameters: m.parameters(),
        matched_words: matched,
        gated: m.gated(),
        private: m.private,
        note,
    }
}

const NO_FILE: &str = "not available as a file lmgw can run yet";

/// The hub's budget after the answer, and whether the next search may meet
/// it.
fn budget(found: &Found, token_set: bool) -> Option<HubBudget> {
    let b = found.budget;
    if found.calls == 0 && b.remaining.is_none() && b.reset_s.is_none() {
        return None;
    }
    let near = b
        .remaining
        .is_some_and(|r| r < u64::from(found.calls.max(1)));
    let limited = found.answer == FindAnswer::RateLimited;
    let sentence = (near || limited).then(|| {
        let allows = allowance(&b, token_set);
        let wait = b.reset_s.map(duration_words);
        match limited {
            true => format!(
                "Hugging Face asks lmgw to wait {}.{allows}",
                wait.unwrap_or_else(|| "a little".into())
            ),
            false => format!(
                "Hugging Face's limit resets in {}: {} requests are left, and this search took \
                 {}.{allows}",
                wait.unwrap_or_else(|| "a little while".into()),
                b.remaining.unwrap_or(0),
                found.calls
            ),
        }
    });
    Some(HubBudget {
        remaining: b.remaining,
        reset_s: b.reset_s,
        quota: b.quota,
        window_s: b.window_s,
        calls: found.calls,
        near,
        sentence,
        allowance: Some(allowance(&b, token_set).trim().to_string()).filter(|a| !a.is_empty()),
    })
}

/// " Without an account it allows 500 requests every 5 minutes; a Hugging
/// Face token in Settings raises that." — from the last policy the hub
/// sent; empty without one.
fn allowance(b: &crate::hf::search::Budget, token_set: bool) -> String {
    let (Some(q), Some(w)) = (b.quota, b.window_s) else {
        return String::new();
    };
    let every = match w {
        w if w % 60 == 0 && w >= 120 => format!("{} minutes", w / 60),
        60 => "minute".into(),
        w => format!("{w} seconds"),
    };
    match token_set {
        true => format!(" With your Hugging Face token it allows {q} requests every {every}."),
        false => format!(
            " Without an account it allows {q} requests every {every}; a Hugging Face token in \
             Settings raises that."
        ),
    }
}

/// The answer's sentence (§4.5).
fn sentence(found: &Found, card: Option<&ModelCard>, token_set: bool) -> String {
    let name = found
        .origin
        .as_ref()
        .map(|o| display(o.id()))
        .unwrap_or_else(|| found.query.clone());
    match found.answer {
        FindAnswer::Card => match card {
            Some(c) => match &c.by_user {
                Some(id) if !c.by.eq_ignore_ascii_case(id) => {
                    format!("{} by {} ({id}).", c.name, c.by)
                }
                _ => format!("{} by {}.", c.name, c.by),
            },
            None => String::new(),
        },
        FindAnswer::NoMatch => match &found.missing_repo {
            Some(repo) => format!(
                "lmgw found no model at huggingface.co/{repo}. It may be private to another \
                 account, or the address may be mistyped."
            ),
            None => format!(
                "lmgw found no model called \u{201c}{}\u{201d} that it can run. {DIGITS_HINT}",
                found.query
            ),
        },
        FindAnswer::NoRunnableFile => format!(
            "lmgw found {name} on Hugging Face, but nobody has made a file of it that lmgw can \
             run yet. {DIGITS_HINT}"
        ),
        FindAnswer::LooseOnly => format!(
            "lmgw found no model whose name has every word of \u{201c}{}\u{201d}. Did you mean \
             one of these? {DIGITS_HINT}",
            found.query
        ),
        FindAnswer::AllGated => format!(
            "The maker of {name} asks people to accept its terms on Hugging Face first: \
             huggingface.co/{}. Accept them there, then set a Hugging Face token under \
             Settings \u{2192} Tokens & updates{}.",
            found.gated_upload.as_deref().unwrap_or_default(),
            if token_set {
                " (the one set now can't read it yet)"
            } else {
                ""
            }
        ),
        FindAnswer::NothingFits => nothing_fits(found, card, &name),
        FindAnswer::NoFileInBand => {
            let (upload, what) = match &found.outside_band {
                Some(c) => (c.upload.id.as_str(), walk::outside_band(&c.files)),
                None => ("", String::new()),
            };
            format!(
                "lmgw found {name} on Hugging Face, but none of its files is one lmgw picks by \
                 itself: every file is {what}. {} The files are listed at \
                 huggingface.co/{upload}.",
                walk::band_words()
            )
        }
        FindAnswer::OtherKind => format!(
            "{name} is a {} model on Hugging Face, which Find a model doesn't set up yet. \
             Picture models are added from Models \u{2192} Add from recipe, speech models from \
             the audio catalog.",
            found
                .origin
                .as_ref()
                .and_then(|o| o.model.pipeline_tag.clone())
                .unwrap_or_default()
        ),
        FindAnswer::Unreachable if found.walk_unanswered => format!(
            "Hugging Face answered, but not when lmgw asked for the files of {name}. Try \
             again in a while; what it answered is under the details."
        ),
        FindAnswer::Unreachable => {
            "lmgw can't reach Hugging Face right now. Check the internet connection and try \
             again."
                .into()
        }
        FindAnswer::RateLimited => budget(found, token_set)
            .and_then(|b| b.sentence)
            .unwrap_or_else(|| "Hugging Face asks lmgw to wait a little.".into()),
        FindAnswer::NotHuggingFace => match &found.parsed {
            Query::Elsewhere { host } => {
                format!("lmgw finds models on Hugging Face. This address is on {host}.")
            }
            _ => "lmgw finds models on Hugging Face.".into(),
        },
        FindAnswer::NotAModel => match &found.parsed {
            Query::HubPage(page) => hub_page_sentence(page),
            _ => "This Hugging Face page isn't a model's.".into(),
        },
        FindAnswer::More => String::new(),
    }
}

/// What a hub page that isn't a model's is, in a sentence (§4.1).
fn hub_page_sentence(page: &HubPage) -> String {
    const PASTE: &str = "Type a model's name, or paste the address of a model's page.";
    match page {
        HubPage::Account { address, name } => format!(
            "{address} is the page of {name} on Hugging Face, which lists its models, not one \
             model. Open one of them and paste its address, or type the model's name."
        ),
        HubPage::Dataset { address } => format!(
            "{address} is a dataset: data people train or test models with, not a model lmgw \
             can run. {PASTE}"
        ),
        HubPage::Space { address } => format!(
            "{address} is a Space: an app someone runs on Hugging Face, not a model lmgw can \
             download. {PASTE}"
        ),
        HubPage::Other { address } => format!("{address} isn't a model's page. {PASTE}"),
    }
}

/// `NothingFits`' sentence: the model closest to fitting, its size and the
/// card's (D17, refined in the review round).
fn nothing_fits(found: &Found, card: Option<&ModelCard>, name: &str) -> String {
    let Some(c) = card.filter(|c| c.fit.capacity_bytes > 0 && c.fit.uses_bytes > 0) else {
        return format!("{name} doesn't fit your graphics card.");
    };
    let lead = match (found.stopped.is_some(), found.walk_len) {
        (true, _) => format!(
            "Of the models by {} with this name that lmgw could check, {name} comes closest \
             to fitting your graphics card, and it doesn't fit: ",
            c.by
        ),
        (false, n) if n > 1 => format!(
            "None of the {n} models by {} with this name fits your graphics card. The one \
             closest to fitting is {name}: ",
            c.by
        ),
        _ => format!("{name} doesn't fit your graphics card: "),
    };
    let needs = fmt_bytes(c.fit.uses_bytes.saturating_add(c.fit.headroom_bytes));
    let what = match c.file.is_empty() {
        true => format!(
            "at 4 bits per weight, the narrowest file lmgw picks, it needs about {needs} with \
             the headroom"
        ),
        false => format!(
            "its smallest file, {}, needs {needs} with the headroom",
            c.quant.as_deref().unwrap_or(&c.file)
        ),
    };
    format!(
        "{lead}{what}, and the card has {}.",
        fmt_bytes(c.fit.capacity_bytes)
    )
}

/// The projector D121 picks among the tree's, and the first drafter.
fn companions(chosen: &Chosen, kind: &Kind) -> Vec<(HfCompanion, Option<u64>)> {
    let role = |p: &str| crate::ops::classify_repo_file(p, "chat");
    let projectors: Vec<(&str, u64)> = chosen
        .tree
        .iter()
        .filter(|f| role(&f.path) == "mmproj")
        .map(|f| (f.path.as_str(), f.size.unwrap_or(u64::MAX)))
        .collect();
    let mut out = Vec::new();
    if let Some(p) = hf::pick_projector(projectors.iter().copied()) {
        let size = chosen
            .tree
            .iter()
            .find(|f| f.path == p)
            .and_then(|f| f.size);
        out.push((
            HfCompanion {
                file: p.to_string(),
                role: "mmproj".into(),
            },
            size,
        ));
    }
    if *kind == Kind::Chat {
        let mut drafters: Vec<&hf::HfFile> = chosen
            .tree
            .iter()
            .filter(|f| role(&f.path) == "drafter")
            .collect();
        drafters.sort_by(|a, b| a.path.cmp(&b.path));
        if let Some(d) = drafters.first() {
            out.push((
                HfCompanion {
                    file: d.path.clone(),
                    role: "drafter".into(),
                },
                d.size,
            ));
        }
    }
    out
}

/// The files the card shows beside what runs now and on the whole card.
/// The address's file is both picks when it fits the whole card, and the
/// beside pick only when it fits there too (D77, D120): it is never swapped
/// for another file that fits beside. Otherwise the size picks.
fn picks(chosen: &Chosen, beside: Option<Room>, whole: Option<Room>) -> [Option<WeightsFile>; 2] {
    match chosen.asked().filter(|a| walk::fits(a, whole)) {
        Some(a) => [walk::fits(a, beside).then(|| a.clone()), Some(a.clone())],
        None => [
            walk::pick_by_size(&chosen.files, beside).cloned(),
            walk::pick_by_size(&chosen.files, whole).cloned(),
        ],
    }
}

/// "Why this file".
fn why(file: &WeightsFile, chosen: &Chosen, origin: &Original, asked: bool) -> String {
    let label = hf::quant_label(&file.file).unwrap_or_else(|| file.file.clone());
    let rule = if asked {
        "the file the address names".to_string()
    } else {
        "the largest file of at least 4 bits per weight, up to the Q8 class, that fits by its \
         size"
            .to_string()
    };
    let width = match (file.bits, origin.model.parameters()) {
        (Some(b), Some(p)) => format!(
            " It is {b:.1} bits per weight over the whole file, by the {} parameters Hugging \
             Face lists.",
            parameters_words(p)
        ),
        _ => " Hugging Face lists no parameter count for it, so its width isn't known before \
              the header is read."
            .into(),
    };
    format!(
        "{label} from {}: {rule}.{width} lmgw hasn't read the file's header yet; reading it \
         (1 to 16 MiB) checks the fit exactly and may pick another file.",
        chosen.upload.id
    )
}

/// The card's file and its fits by size.
struct Shown {
    file: Option<WeightsFile>,
    fit: FileFit,
    whole_card: Option<CardFile>,
}

fn shown(
    chosen: &Chosen,
    card: Option<&CardNow>,
    why_line: impl Fn(&WeightsFile) -> String,
) -> Shown {
    let headroom = card.map_or(0, |c| c.card.headroom);
    let room = |m: FitMeasure| {
        card.map(|c| Room {
            capacity: c.room(m),
            headroom,
        })
    };
    let [beside, whole] = picks(
        chosen,
        room(FitMeasure::Beside),
        room(FitMeasure::WholeCard),
    );
    let by = |f: &WeightsFile, m: FitMeasure| fit::by_size(f.bytes, card, m, why_line(f));
    match (beside, whole) {
        (Some(b), whole) => {
            let other = whole.filter(|w| w.file != b.file).map(|w| CardFile {
                quant: hf::quant_label(&w.file),
                bytes: w.bytes.unwrap_or(0),
                fit: by(&w, FitMeasure::WholeCard),
                file: w.file,
            });
            Shown {
                fit: by(&b, FitMeasure::Beside),
                file: Some(b),
                whole_card: other,
            }
        }
        (None, Some(w)) => {
            let mut f = by(&w, FitMeasure::WholeCard);
            f.notes.push(
                "It fits once what else is on the graphics card now is closed or unloaded.".into(),
            );
            Shown {
                fit: f,
                file: Some(w),
                whole_card: None,
            }
        }
        (None, None) => {
            // Nothing fits: the smallest file, marked against the whole card.
            let small = walk::smallest(&chosen.files).map(|(f, _)| f.clone());
            let fit = match &small {
                Some(f) => by(f, FitMeasure::WholeCard),
                None => fit::by_size(
                    None,
                    card,
                    FitMeasure::WholeCard,
                    format!(
                        "lmgw picks no file of this upload by its size: every file is {}",
                        walk::outside_band(&chosen.files)
                    ),
                ),
            };
            Shown {
                file: small,
                fit,
                whole_card: None,
            }
        }
    }
}

/// The card for `found`'s original (`Card` and `NothingFits`).
fn card(found: &Found, inputs: &Inputs, lists: &Lists) -> Option<ModelCard> {
    let origin = found.origin.as_ref()?;
    let kind = kind_dto(&origin.kind);
    let name = display(origin.id());
    let by = found
        .by
        .clone()
        .unwrap_or_else(|| origin.model.author().to_string());
    // A person's display name is self-chosen: the id rides beside it (D19).
    let by_user = found.by_user.then(|| origin.model.author().to_string());
    let licence = Some(ModelLicence {
        id: origin.model.licence().map(str::to_string),
        name: None,
        link: Some(format!("{}/{}", hf::HF_BASE, origin.id())),
    });
    let mut c = ModelCard {
        name: name.clone(),
        by,
        by_user,
        origin: origin.id().to_string(),
        likes: origin.model.likes,
        downloads: origin.model.downloads,
        kind,
        parameters: origin.model.parameters(),
        licence,
        disk_free_bytes: inputs.disk_free,
        private: origin.model.private,
        origin_private: origin.model.private,
        gated: origin.model.gated(),
        ..ModelCard::default()
    };
    let Some(chosen) = &found.chosen else {
        // Passed over with no call: its size at 4 bits per weight.
        let estimate = origin
            .model
            .parameters()
            .map(|p| (p as f64 * FLOOR_BITS / 8.0) as u64);
        c.fit = fit::by_size(
            estimate,
            inputs.card.as_ref(),
            FitMeasure::WholeCard,
            match origin.model.parameters() {
                Some(p) => format!(
                    "its size is estimated from the {} parameters Hugging Face lists, at {} bits \
                     per weight, the narrowest file lmgw recommends",
                    parameters_words(p),
                    FLOOR_BITS
                ),
                None => "Hugging Face lists no size for it".into(),
            },
        );
        c.why =
            "lmgw didn't ask for this model's files: even its narrowest file wouldn't fit.".into();
        return Some(c);
    };
    let asked = chosen.asked_file.is_some();
    let s = shown(chosen, inputs.card.as_ref(), |f| {
        format!(
            "marked by its size alone: lmgw hasn't read {}'s header yet",
            hf::quant_label(&f.file).unwrap_or_else(|| f.file.clone())
        )
    });
    c.upload = chosen.upload.id.clone();
    c.private = c.private || chosen.upload.private;
    c.gated = chosen.upload.gated();
    c.fit = s.fit;
    c.whole_card = s.whole_card;
    let comps = companions(chosen, &origin.kind);
    if comps.iter().any(|(c, _)| c.role == "mmproj") {
        c.can.push("reads pictures".into());
    }
    let comp_bytes: u64 = comps.iter().filter_map(|(_, b)| *b).sum();
    c.companions = comps.into_iter().map(|(c, _)| c).collect();
    if let Some(f) = &s.file {
        let is_asked = asked && chosen.asked_file.as_deref() == Some(f.file.as_str());
        c.file = f.file.clone();
        c.named_file = is_asked.then(|| f.file.clone());
        c.quant = hf::quant_label(&f.file);
        c.parts = f.parts;
        c.why = why(f, chosen, origin, is_asked);
        c.download = hf::rate::estimate(
            f.bytes.unwrap_or(0).saturating_add(comp_bytes),
            &inputs.engine,
            inputs.downloads_rate,
            &[],
        );
        if !c.private {
            let url = share_url(&chosen.upload.id, &f.file);
            c.share = Some(ShareLink {
                text: share_text(&share_name(&c, &chosen.upload.id), &url),
                url,
            });
        }
        if let Some(a) = chosen.asked().filter(|_| !is_asked) {
            let room = inputs
                .card
                .as_ref()
                .map(|k| (k.room(FitMeasure::WholeCard), k.card.headroom));
            c.fit.notes.push(match (a.bytes, room) {
                (Some(b), Some((room, headroom))) => format!(
                    "{}, the file the address names, needs {} with the headroom, and the card \
                     has {}, so the card shows the file of the same upload that fits by its \
                     size",
                    a.file,
                    fmt_bytes(b.saturating_add(headroom)),
                    fmt_bytes(room)
                ),
                _ => format!(
                    "{}, the file the address names, has no size Hugging Face lists in full, \
                     so the card shows the file of the same upload that fits by its size",
                    a.file
                ),
            });
        }
    }
    let upload = |u: &HubModel, plain: bool| UploadChoice {
        repo: u.id.clone(),
        likes: u.likes,
        downloads: u.downloads,
        gated: u.gated(),
        private: u.private,
        plain,
    };
    let uploads: Vec<UploadChoice> = chosen
        .other_plain
        .iter()
        .map(|u| upload(u, true))
        .chain(chosen.renamed.iter().map(|u| upload(u, false)))
        .collect();
    (c.other_uploads, c.other_uploads_total, c.other_uploads_more) = window(
        uploads,
        lists.other_uploads_offset,
        lists.other_uploads_count,
    );
    c.more_uploads = chosen.more_uploads;
    Some(c)
}

/// `found` as `model_find`'s answer, measured with `inputs`, with the part
/// of its lists `lists` asks for.
pub fn answer(found: &Found, inputs: &Inputs, lists: &Lists) -> ModelFind {
    let card = match found.answer {
        FindAnswer::Card | FindAnswer::NothingFits => card(found, inputs, lists),
        _ => None,
    };
    let card_origin = card.as_ref().map(|c| c.origin.to_ascii_lowercase());
    let others = match found.answer {
        // An address names one model: there are no others.
        _ if !matches!(found.parsed, Query::Name(_)) => Vec::new(),
        _ => found
            .ranked
            .originals
            .iter()
            .filter(|o| Some(o.id().to_ascii_lowercase()) != card_origin)
            .map(|o| {
                choice(
                    &o.model,
                    o.matched.matched() as u32,
                    found.no_file.contains(o.id()).then(|| NO_FILE.to_string()),
                )
            })
            .collect(),
    };
    let did_you_mean = found
        .ranked
        .partial
        .iter()
        .map(|(m, matched)| choice(m, matched.matched() as u32, None))
        .collect();
    let (others, others_total, others_more) =
        window(others, lists.others_offset, lists.others_count);
    let (did_you_mean, did_you_mean_total, did_you_mean_more) = window(
        did_you_mean,
        lists.did_you_mean_offset,
        lists.did_you_mean_count,
    );
    ModelFind {
        query: found.query.clone(),
        sentence: sentence(found, card.as_ref(), inputs.token_set),
        card,
        others,
        others_total,
        others_more,
        did_you_mean,
        did_you_mean_total,
        did_you_mean_more,
        answer: found.answer,
        next_cursor: found.next_cursor.clone(),
        hub_budget: budget(found, inputs.token_set),
        notes: found.notes.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn words_for_counts_and_names() {
        assert_eq!(parameters_words(27_000_000_000), "27 billion");
        assert_eq!(parameters_words(600_000_000), "600 million");
        assert_eq!(parameters_words(1_234_000_000_000), "1.2 trillion");
        assert_eq!(display("maker-demo/Demo-2-8B-it"), "Demo 2 8B it");
        assert_eq!(duration_words(40), "40 seconds");
        assert_eq!(duration_words(95), "about 2 minutes");
    }

    /// §7's link survives any file name: each segment percent-encoded
    /// (`lmgw_api_types::model_find::share_url`, the dashboard's too), and
    /// the Find box reads it back as the same file, bare and inside the
    /// share sentence with or without its line break (WP7, D109).
    #[test]
    fn the_share_link_reads_back_as_the_same_file() {
        for file in [
            "sub dir/A b#c?.gguf",
            "Model[Q4].gguf",
            "a(b)'c|d^e\"f<g>`h.gguf",
            "D\u{e9}mo%41~1.gguf",
        ] {
            let url = share_url("fan-demo/Demo-GGUF", file);
            let want = Query::Address {
                repo: "fan-demo/Demo-GGUF".into(),
                file: Some(file.into()),
                rev: Some("main".into()),
            };
            assert_eq!(super::super::query::parse(&url), want, "{url}");
            let text = share_text("Demo", &url);
            assert_eq!(super::super::query::parse(&text), want, "{text}");
            let one_line = text.replace('\n', "");
            assert_eq!(super::super::query::parse(&one_line), want, "{one_line}");
        }
    }

    /// A named file that fits the whole card but not beside what runs now
    /// is the card's file, on the whole card with D72's words, and never
    /// swapped for a file that fits beside (D77, D120).
    #[test]
    fn a_named_file_on_the_whole_card_isnt_swapped() {
        use crate::find::rank::Kind;
        use crate::hf::HfFile;
        use crate::modelinfo::fit::Card;
        const GIB: u64 = 1 << 30;
        let file = |p: &str, gib: u64| HfFile {
            path: p.into(),
            size: Some(gib * GIB),
            kind: "file".into(),
            lfs: None,
        };
        let tree = vec![file("m-Q4_K_M.gguf", 5), file("m-Q8_0.gguf", 9)];
        let files = walk::weights_files(&tree, Some(8_000_000_000));
        let mut found = Found::new(
            "x",
            Query::Address {
                repo: "fan-demo/M-GGUF".into(),
                file: Some("m-Q8_0.gguf".into()),
                rev: None,
            },
        );
        found.answer = FindAnswer::Card;
        found.origin = Some(Original {
            model: HubModel {
                id: "maker-demo/M".into(),
                ..HubModel::default()
            },
            matched: Default::default(),
            kind: Kind::Chat,
            makers_gguf: None,
        });
        found.chosen = Some(Chosen {
            upload: HubModel {
                id: "fan-demo/M-GGUF".into(),
                ..HubModel::default()
            },
            tree,
            files,
            other_plain: Vec::new(),
            renamed: Vec::new(),
            more_uploads: false,
            asked_file: Some("m-Q8_0.gguf".into()),
        });
        // 16 GiB in use by other programs: 8 GiB beside, 24 on the card.
        let inputs = Inputs {
            card: Some(CardNow {
                card: Card {
                    capacity: 24 * GIB,
                    headroom: GIB,
                    budget: true,
                    admission: true,
                },
                hold: false,
                outside: Some(16 * GIB),
                residents: Vec::new(),
                left_out: Vec::new(),
            }),
            downloads_rate: None,
            engine: Engine::NotKnown("a test".into()),
            disk_free: None,
            token_set: false,
        };
        let c = answer(&found, &inputs, &Lists::default()).card.unwrap();
        assert_eq!(c.file, "m-Q8_0.gguf");
        assert_eq!(c.fit.measure, FitMeasure::WholeCard);
        assert!(c.whole_card.is_none(), "{c:?}");
        let notes = &c.fit.notes;
        assert!(
            notes
                .iter()
                .any(|n| n.starts_with("It fits once what else")),
            "{notes:?}"
        );
        assert!(
            !notes
                .iter()
                .any(|n| n.contains("the file the address names")),
            "{notes:?}"
        );
    }

    #[test]
    fn a_window_of_a_list_says_how_many_more() {
        let all: Vec<ModelChoice> = (0..25)
            .map(|i| ModelChoice {
                origin: format!("o/m{i}"),
                ..ModelChoice::default()
            })
            .collect();
        let (part, total, more) = window(all.clone(), 0, 10);
        assert_eq!((part.len(), total, more), (10, 25, 15));
        let (part, _, more) = window(all.clone(), 20, 10);
        assert_eq!((part[0].origin.as_str(), part.len(), more), ("o/m20", 5, 0));
        let (part, total, more) = window(all, 30, 10);
        assert_eq!((part.len(), total, more), (0, 25, 0));
    }
}
