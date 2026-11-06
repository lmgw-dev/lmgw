//! The Chat's image and audio tools on the page (chat-media-tools design §8):
//! a running call's live pane (`live`), the card under each produced
//! picture with its facts (`card`) and its actions (`actions`), and the
//! player of a spoken result (`player`).
//!
//! The files a call stored are the message's own attachments (`source`
//! `tool`, its `call_id`): the stream's `result` frame hands them over
//! ([`take_files`]), a stored thread lists them on the reply. A card
//! action's pictures are attachments of the same message (`source`
//! `action`, `meta.media.parent` what they were made from), so a picture's
//! card gathers its whole family from the message's list ([`family_of`]).

mod actions;
mod card;
mod live;
mod player;

use leptos::prelude::*;
use lmgw_api_types::chat_media::{
    MediaFacts, MediaPhase, ToolProgressFrame, SOURCE_ACTION, SOURCE_TOOL,
};
use lmgw_api_types::chat_threads::{AttachmentFacts, AttachmentMeta};
use serde_json::Value;

use super::chat::ToolCard;
use super::chat_stream::VoiceEvent;
use lmgw_ui_kit::scope::Scope;
use lmgw_ui_kit::widgets::Toasts;

pub(super) use live::MediaLive;

/// What a media card reaches of the Chat page. Provided once by the page.
#[derive(Clone, Copy)]
pub(super) struct MediaCtx {
    /// The open thread: the media cancel route names it.
    pub thread: Memo<Option<i64>>,
    /// The shared viewer, full size.
    pub open: Callback<AttachmentMeta>,
    /// Copy a file into the composer as a draft (Attach).
    pub attach: Callback<AttachmentMeta>,
    /// The pictures Attach is copying: their Attach is held until it is done.
    pub attaching: RwSignal<Vec<i64>>,
    /// The card actions that run or ran on this page (`actions::Run`): kept
    /// here rather than on the card, so a card that is left (another thread
    /// opened) and shown again still has its running action.
    runs: RwSignal<Vec<actions::Run>, LocalStorage>,
    pub scope: Scope,
    pub toasts: Toasts,
    /// The transport the host handed the page.
    pub http: lmgw_ui_kit::http::Http,
}

impl MediaCtx {
    pub fn new(
        thread: Memo<Option<i64>>,
        open: Callback<AttachmentMeta>,
        attach: Callback<AttachmentMeta>,
        attaching: RwSignal<Vec<i64>>,
        scope: Scope,
        toasts: Toasts,
    ) -> Self {
        Self {
            thread,
            open,
            attach,
            attaching,
            runs: RwSignal::new_local(Vec::new()),
            scope,
            toasts,
            http: lmgw_ui_kit::http::use_http(),
        }
    }

    /// The page goes: the actions' streams are closed (the actions run on
    /// in the gateway, as they do when a stream is dropped).
    pub fn close_streams(self) {
        self.runs
            .try_with_untracked(|v| v.iter().for_each(actions::Run::abort));
    }
}

pub(super) fn provide(ctx: MediaCtx) {
    provide_context(ctx);
}

fn use_media() -> MediaCtx {
    expect_context::<MediaCtx>()
}

/// Whether a tool is one of the gateway's image or audio tools.
pub(super) fn is_media_tool(name: &str) -> bool {
    name.starts_with("image__") || name.starts_with("audio__")
}

/// A `progress` frame: the running call it names (by the id its `ready`
/// frame gave its card) takes it — the newest card with that id that has no
/// result yet, should a reply carry an id twice. A frame for a call no card
/// knows yet is dropped: progress is lossy, and the next one supersedes it.
pub(super) fn progress(tools: RwSignal<Vec<ToolCard>>, f: &ToolProgressFrame) {
    let card = tools.with_untracked(|t| {
        t.iter()
            .rev()
            .find(|c| {
                !c.done.get_untracked()
                    && c.call_id
                        .with_untracked(|id| id.as_deref() == Some(&f.call_id))
            })
            .copied()
    });
    let Some(card) = card else { return };
    let now = now_ms();
    card.media
        .update(|m| m.get_or_insert_with(MediaLive::default).apply(f, now));
}

/// The turn's stream ended — done, stopped or failed — with cards still
/// running: no result can come for them any more. Each is settled as
/// stopped, and its live pane, preview and stages go.
pub(super) fn settle(tools: RwSignal<Vec<ToolCard>>) {
    // The reply may be off the page by now (its thread left and dropped).
    tools.try_with_untracked(|t| {
        for c in t
            .iter()
            .filter(|c| c.done.try_get_untracked() == Some(false))
        {
            c.media.try_set(None);
            if c.output.try_with_untracked(String::is_empty) == Some(true) {
                c.output.try_set(super::chat_approvals::STOPPED.to_string());
            }
            c.done.try_set(true);
        }
    });
}

/// The chat model's alias when `ev` is its `loading` after a media call of
/// the turn ran: the loop letting its claim go for the picture and taking it
/// back (design §5.3), not the turn's first load.
pub(super) fn regain_alias(ev: &VoiceEvent, tools: RwSignal<Vec<ToolCard>>) -> Option<String> {
    let VoiceEvent::State(v) = ev else {
        return None;
    };
    if v["stage"] != "chat" || v["state"] != "loading" {
        return None;
    }
    let after_media = tools.with_untracked(|t| {
        t.iter()
            .any(|c| c.done.get_untracked() && c.name.with_untracked(|n| is_media_tool(n)))
    });
    after_media.then(|| v["alias"].as_str().unwrap_or("the chat model").to_string())
}

/// The page's clock (ms since the epoch); none off the browser (the unit
/// tests), where no pane's clock ticks.
fn now_ms() -> f64 {
    #[cfg(target_arch = "wasm32")]
    {
        js_sys::Date::now()
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        0.0
    }
}

/// A result's stored files join the message's, each once. A message no
/// longer on the page (another thread opened) takes nothing.
pub(super) fn take_files(files: RwSignal<Vec<AttachmentMeta>>, new: &[AttachmentMeta]) {
    if new.is_empty() {
        return;
    }
    files.try_update(|v| {
        for a in new {
            if !v.iter().any(|x| x.id == a.id) {
                v.push(a.clone());
            }
        }
    });
}

/// How a tool result's resource block reads on the card: a stored file by
/// its ref (`image att:57`), anything else by its kind.
pub(super) fn resource_name(b: &Value) -> String {
    let uri = b["uri"].as_str().unwrap_or("");
    match uri.strip_prefix("lmgw:attachment/") {
        Some(n) if n.parse::<i64>().is_ok() => {
            let kind = b["mime"]
                .as_str()
                .and_then(|m| m.split('/').next())
                .filter(|k| matches!(*k, "image" | "audio"))
                .unwrap_or("file");
            format!("{kind} att:{n}")
        }
        _ => "[resource]".to_string(),
    }
}

/// A produced file's facts (`meta.media`); empty for any other file.
pub(super) fn facts(a: &AttachmentMeta) -> MediaFacts {
    serde_json::from_value::<AttachmentFacts>(a.meta.clone())
        .ok()
        .and_then(|f| f.media)
        .unwrap_or_default()
}

/// What a reply's tool card says of its files: its call's id, and its
/// result's text, which names each file it made (`att:N`).
#[derive(Debug, Clone, PartialEq)]
pub(super) struct CardKey {
    pub call_id: Option<String>,
    pub output: String,
}

impl CardKey {
    pub fn of(c: &ToolCard) -> Self {
        Self {
            call_id: c.call_id.get(),
            output: c.output.get(),
        }
    }
}

/// The refs a result's text names: `att:57` → 57.
fn named_refs(text: &str) -> Vec<i64> {
    text.match_indices("att:")
        .filter_map(|(at, _)| {
            let digits: String = text[at + 4..]
                .chars()
                .take_while(char::is_ascii_digit)
                .collect();
            digits.parse().ok()
        })
        .collect()
}

/// The files card `i` of a reply's `cards` made, in the order they were
/// stored: its call's files its result names, when it names any (every
/// image and speech result does); else its call's files, where a reply
/// that carries the id on several cards (one stored before ids were
/// unique, or an upstream that repeats one) gives each card one, in order.
pub(super) fn roots_of(
    files: &[AttachmentMeta],
    cards: &[CardKey],
    i: usize,
) -> Vec<AttachmentMeta> {
    let Some(card) = cards.get(i) else {
        return Vec::new();
    };
    let Some(cid) = card.call_id.as_deref() else {
        return Vec::new();
    };
    let mine = files
        .iter()
        .filter(|a| a.source == SOURCE_TOOL && a.call_id.as_deref() == Some(cid));
    let named = named_refs(&card.output);
    if mine.clone().any(|a| named.contains(&a.id)) {
        return mine.filter(|a| named.contains(&a.id)).cloned().collect();
    }
    let same: Vec<usize> = (0..cards.len())
        .filter(|&j| cards[j].call_id.as_deref() == Some(cid))
        .collect();
    if same.len() < 2 {
        return mine.cloned().collect();
    }
    let nth = same.iter().position(|&j| j == i).unwrap_or(0);
    mine.skip(nth).take(1).cloned().collect()
}

/// Picture `root` and every card action's picture made from it, or from one
/// of those, oldest first.
pub(super) fn family_of(files: &[AttachmentMeta], root: i64) -> Vec<AttachmentMeta> {
    let mut ids = vec![root];
    // Parents come before their children (ids rise), but a pass per
    // generation does not rely on it.
    loop {
        let before = ids.len();
        for a in files {
            if a.source == SOURCE_ACTION
                && !ids.contains(&a.id)
                && facts(a).parent.is_some_and(|p| ids.contains(&p))
            {
                ids.push(a.id);
            }
        }
        if ids.len() == before {
            break;
        }
    }
    let mut fam: Vec<AttachmentMeta> = files
        .iter()
        .filter(|a| ids.contains(&a.id))
        .cloned()
        .collect();
    fam.sort_by_key(|a| a.id);
    fam
}

/// The produced files of a message no card shows: a call whose card is not
/// there (its record holds another id), or an action whose source is not
/// one of this message's pictures (a re-roll of an edit names the edit's
/// input as its parent).
pub(super) fn loose(files: &[AttachmentMeta], cards: &[CardKey]) -> Vec<AttachmentMeta> {
    let shown: Vec<i64> = (0..cards.len())
        .flat_map(|i| roots_of(files, cards, i))
        .flat_map(|r| family_of(files, r.id))
        .map(|a| a.id)
        .collect();
    files
        .iter()
        .filter(|a| {
            (a.source == SOURCE_TOOL || a.source == SOURCE_ACTION)
                && !shown.contains(&a.id)
                // An action picture whose parent is loose too is in that
                // one's family.
                && !(a.source == SOURCE_ACTION
                    && facts(a).parent.is_some_and(|p| {
                        files.iter().any(|x| x.id == p && (x.source == SOURCE_TOOL || x.source == SOURCE_ACTION))
                    }))
        })
        .cloned()
        .collect()
}

/// Seconds, as a person reads a wait: `0.8 s`, `4.7 s`, `1 min 12 s`.
pub(super) fn secs(ms: u64) -> String {
    if ms < 60_000 {
        format!("{:.1} s", ms as f64 / 1000.0)
    } else {
        format!("{} min {} s", ms / 60_000, (ms % 60_000) / 1000)
    }
}

/// The words of a stage (design §5.3): `Unloading chat/qwen3-30b`,
/// `Loading image/z-image-turbo`, `Waiting for room (needs 6.6 GiB, 2.1 GiB
/// free)`, `Generating`.
pub(super) fn stage_words(f: &ToolProgressFrame) -> String {
    let model = f.model.as_deref().unwrap_or("the model");
    match f.phase {
        // The machine-check design's words (D14), as every surface says
        // them; the live pane draws its bar under them.
        Some(MediaPhase::Downloading) => lmgw_ui_kit::download::Dl::of_frame(f)
            .unwrap_or_default()
            .words(),
        Some(MediaPhase::Unloading) => format!("Unloading {model}"),
        Some(MediaPhase::Loading) => format!("Loading {model}"),
        Some(MediaPhase::WaitingForRoom) => match (f.needs_bytes, f.free_bytes) {
            (Some(n), Some(free)) => {
                format!(
                    "Waiting for room (needs {}, {} free)",
                    lmgw_api_types::units::fmt_bytes(n),
                    lmgw_api_types::units::fmt_bytes(free)
                )
            }
            _ => "Waiting for room".to_string(),
        },
        Some(MediaPhase::Queued) => match f.queue_position {
            Some(p) if p > 0 => format!("Queued ({p} ahead)"),
            _ => "Queued".to_string(),
        },
        Some(MediaPhase::Generating) => "Generating".to_string(),
        Some(MediaPhase::Speaking) => format!("Speaking with {model}"),
        Some(MediaPhase::Transcribing) => format!("Transcribing with {model}"),
        Some(MediaPhase::Upscaling) => format!("Upscaling with {model}"),
        None => "Working".to_string(),
    }
}

/// A picture frame's style: its aspect ratio, which the stylesheet sizes it
/// by (`.media-frame`), and its width in pixels, which it is never drawn
/// wider than.
pub(super) fn frame_style(w: u32, h: u32) -> String {
    if w > 0 && h > 0 {
        format!("--ar: {:.4}; --nw: {w}px", w as f64 / h as f64)
    } else {
        "--ar: 1".to_string()
    }
}

/// The picture's width and height a call asked for (`size: "1024x1024"`, or
/// `width`/`height`), for the preview's box before the first frame.
pub(super) fn asked_size(args: &str) -> Option<(u32, u32)> {
    let v: Value = serde_json::from_str(args).ok()?;
    if let Some((w, h)) = v["size"].as_str().and_then(|s| s.split_once(['x', 'X'])) {
        return Some((w.trim().parse().ok()?, h.trim().parse().ok()?));
    }
    let n = |k: &str| v[k].as_u64().and_then(|n| u32::try_from(n).ok());
    Some((n("width")?, n("height")?))
}

/// The pictures and recordings call `card` made, under its card: its live
/// pane while it runs, then each produced picture's card (with the action
/// pictures made from it) or a spoken result's player. `tools` are the
/// reply's cards; `settled` whether the reply is a stored message, which
/// the card actions need.
#[component]
pub(super) fn CallMedia(
    card: ToolCard,
    tools: RwSignal<Vec<ToolCard>>,
    files: RwSignal<Vec<AttachmentMeta>>,
    #[prop(into)] settled: Signal<bool>,
) -> impl IntoView {
    let running = Memo::new(move |_| {
        !card.done.get()
            && (card.media.with(Option::is_some) || card.name.with(|n| is_media_tool(n)))
    });
    let roots = Memo::new(move |_| {
        let (keys, i) = tools.with(|t| {
            let keys: Vec<CardKey> = t.iter().map(CardKey::of).collect();
            (keys, t.iter().position(|c| c.index == card.index))
        });
        i.map(|i| files.with(|f| roots_of(f, &keys, i)))
            .unwrap_or_default()
    });
    let aspect = Signal::derive(move || card.args.with(|a| asked_size(a)));
    view! {
        <Show when=move || running.get()>
            <live::LivePane
                live=card.media
                call_id=card.call_id
                aspect=aspect
                what=card.name
            />
        </Show>
        <For each=move || roots.get() key=|a| a.id let:root>
            {if root.kind == "audio" {
                view! { <player::AudioResult a=root/> }.into_any()
            } else {
                view! { <card::ImageFamily root=root.id files=files settled=settled/> }.into_any()
            }}
        </For>
    }
}

/// A message's produced files no card of it shows ([`loose`]), each in its
/// own card.
#[component]
pub(super) fn LooseMedia(
    tools: RwSignal<Vec<ToolCard>>,
    files: RwSignal<Vec<AttachmentMeta>>,
    #[prop(into)] settled: Signal<bool>,
) -> impl IntoView {
    let left = Memo::new(move |_| {
        let keys: Vec<CardKey> = tools.with(|t| t.iter().map(CardKey::of).collect());
        files.with(|f| loose(f, &keys))
    });
    view! {
        <For each=move || left.get() key=|a| a.id let:a>
            {if a.kind == "audio" {
                view! { <player::AudioResult a=a/> }.into_any()
            } else {
                view! { <card::ImageFamily root=a.id files=files settled=settled/> }.into_any()
            }}
        </For>
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn att(id: i64, source: &str, call_id: Option<&str>, parent: Option<i64>) -> AttachmentMeta {
        AttachmentMeta {
            id,
            kind: "image".into(),
            source: source.into(),
            call_id: call_id.map(str::to_string),
            meta: match parent {
                Some(p) => json!({"media": {"parent": p}}),
                None => json!({"media": {"tool": "image__generate"}}),
            },
            ..Default::default()
        }
    }

    fn key(call_id: &str, output: &str) -> CardKey {
        CardKey {
            call_id: Some(call_id.into()),
            output: output.into(),
        }
    }

    /// Two calls of one reply under one id (stored before ids were unique,
    /// or an upstream that repeats one): each card shows its own picture —
    /// the one its result names, else the next of the id's in order — never
    /// both.
    #[test]
    fn cards_that_share_an_id_each_show_their_own_files() {
        let files = vec![
            att(5, "tool", Some("call_0"), None),
            att(6, "tool", Some("call_0"), None),
        ];
        let ids = |v: Vec<AttachmentMeta>| v.iter().map(|a| a.id).collect::<Vec<_>>();
        let named = vec![
            key("call_0", "Made image att:5 (512x512) with image/z"),
            key("call_0", "Made image att:6 (512x512) with image/z"),
        ];
        assert_eq!(ids(roots_of(&files, &named, 0)), [5]);
        assert_eq!(ids(roots_of(&files, &named, 1)), [6]);
        let unnamed = vec![key("call_0", "done"), key("call_0", "")];
        assert_eq!(ids(roots_of(&files, &unnamed, 0)), [5]);
        assert_eq!(ids(roots_of(&files, &unnamed, 1)), [6]);
        assert!(loose(&files, &named).is_empty());
        // A card whose call made two files names both.
        let two = vec![key("call_0", "Made images att:5 and att:6")];
        assert_eq!(ids(roots_of(&files, &two, 0)), [5, 6]);
        assert_eq!(named_refs("image att:57, att:3x and att:"), [57, 3]);
    }

    /// A progress frame goes to the newest card with its id that has no
    /// result yet.
    #[test]
    fn progress_reaches_the_running_card_of_a_repeated_id() {
        let owner = Owner::new();
        owner.with(|| {
            let card = |index: i64, done: bool| ToolCard {
                index,
                name: RwSignal::new("image__generate".into()),
                args: RwSignal::new(String::new()),
                output: RwSignal::new(String::new()),
                is_error: RwSignal::new(false),
                ms: RwSignal::new(None),
                done: RwSignal::new(done),
                approval: super::super::chat_approvals::Approval::new(),
                call_id: RwSignal::new(Some("call_0".into())),
                media: RwSignal::new(None),
            };
            let (first, second) = (card(0, true), card(1, false));
            let tools = RwSignal::new(vec![first, second]);
            let f = ToolProgressFrame {
                call_id: "call_0".into(),
                phase: Some(MediaPhase::Generating),
                ..Default::default()
            };
            progress(tools, &f);
            assert!(first.media.get_untracked().is_none());
            assert!(second.media.get_untracked().is_some());
            // The turn ends with the second still running: it is settled.
            settle(tools);
            assert!(second.done.get_untracked());
            assert!(second.media.get_untracked().is_none());
            assert_eq!(
                second.output.get_untracked(),
                super::super::chat_approvals::STOPPED
            );
        });
    }

    #[test]
    fn a_stored_file_is_named_by_its_ref() {
        let b = json!({"type": "resource", "uri": "lmgw:attachment/57", "mime": "image/png"});
        assert_eq!(resource_name(&b), "image att:57");
        let b = json!({"type": "resource", "uri": "lmgw:attachment/9", "mime": "audio/wav"});
        assert_eq!(resource_name(&b), "audio att:9");
        let b = json!({"type": "resource", "uri": "file:///x"});
        assert_eq!(resource_name(&b), "[resource]");
    }

    #[test]
    fn a_family_gathers_the_actions_made_from_its_picture_and_from_those() {
        let files = vec![
            att(1, "upload", None, None),
            att(5, "tool", Some("call_1"), None),
            att(6, "tool", Some("call_2"), None),
            att(7, "action", Some("act-1"), Some(5)),
            att(8, "action", Some("act-2"), Some(7)),
            att(9, "action", Some("act-3"), Some(6)),
            // A re-roll of an edit: its parent is the edit's input, an
            // upload of another message.
            att(10, "action", Some("act-4"), Some(1)),
        ];
        let calls = vec![key("call_1", ""), key("call_2", "")];
        assert_eq!(
            roots_of(&files, &calls, 0)
                .iter()
                .map(|a| a.id)
                .collect::<Vec<_>>(),
            [5]
        );
        assert_eq!(
            family_of(&files, 5)
                .iter()
                .map(|a| a.id)
                .collect::<Vec<_>>(),
            [5, 7, 8]
        );
        assert_eq!(
            family_of(&files, 6)
                .iter()
                .map(|a| a.id)
                .collect::<Vec<_>>(),
            [6, 9]
        );
        assert_eq!(
            loose(&files, &calls)
                .iter()
                .map(|a| a.id)
                .collect::<Vec<_>>(),
            [10]
        );
        // A card that is not there leaves its whole family to one loose card.
        let one = vec![key("call_2", "")];
        assert_eq!(
            loose(&files, &one).iter().map(|a| a.id).collect::<Vec<_>>(),
            [5, 10]
        );
    }

    #[test]
    fn stages_read_as_words() {
        let f = |phase, model: Option<&str>| ToolProgressFrame {
            phase: Some(phase),
            model: model.map(str::to_string),
            ..Default::default()
        };
        assert_eq!(
            stage_words(&f(MediaPhase::Unloading, Some("chat/q"))),
            "Unloading chat/q"
        );
        assert_eq!(
            stage_words(&f(MediaPhase::Loading, Some("image/z"))),
            "Loading image/z"
        );
        let mut w = f(MediaPhase::WaitingForRoom, None);
        w.needs_bytes = Some(7_100_000_000);
        w.free_bytes = Some(2_300_000_000);
        assert_eq!(
            stage_words(&w),
            "Waiting for room (needs 6.6 GiB, 2.1 GiB free)"
        );
        assert_eq!(
            stage_words(&f(MediaPhase::Generating, Some("image/z"))),
            "Generating"
        );
    }

    #[test]
    fn the_asked_size_reads_either_spelling() {
        assert_eq!(asked_size(r#"{"size": "1024x768"}"#), Some((1024, 768)));
        assert_eq!(
            asked_size(r#"{"width": 512, "height": 640}"#),
            Some((512, 640))
        );
        assert_eq!(asked_size(r#"{"prompt": "a cat"}"#), None);
        assert_eq!(secs(4700), "4.7 s");
        assert_eq!(secs(72_000), "1 min 12 s");
    }
}
