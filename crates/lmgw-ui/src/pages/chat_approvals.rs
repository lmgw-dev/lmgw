//! A gated call in the Chat (client-apps design §6): the tool card that
//! waits for a decision, Approve and Decline on it, and the states a call
//! ends in when it does not run.
//!
//! **Where a waiting call comes from.** A gated turn streams, after the
//! cards of its calls, a `tool {event: "approval"}` frame per call that
//! waits and `done {pending_approvals}` ([`hold_for_frame`],
//! [`settle_done`]); a stored reply lists its open calls as
//! `pending_approvals`, which [`cards_of`] pairs with the record's calls.
//! A call without a result that is not itself gated is its sibling: it was
//! held with them and is decided with them.
//!
//! **Deciding** is one request for every waiting call of the reply
//! (`POST …/approvals`): the card's buttons send it when one call waits,
//! and with several they pick a verdict per card and the batch bar sends
//! them. The resumed turn streams into the same message ([`ActionEnv::decide`]).
//!
//! **How a call ends** is read from its result's words, which the gateway
//! writes ([`fate_of`]): declined (with the reason), not decided because a
//! new message came, not run because the turn that was to run it could not
//! start, or run with a result that was never saved.

use leptos::prelude::*;
use leptos::task::spawn_local;
use lmgw_api_types::chat_approvals::{code, ApprovalRequest, MOVED_ON};
use serde_json::{json, Value};

use super::chat::{tools_from_ir, IrTool, ToolCard};
use super::chat_actions::{ActionEnv, MsgOps};
use super::chat_turn::{run_turn, Turn};

/// Why a call has not run yet.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum Hold {
    /// It is gated: it runs once approved.
    Gated { id: String, label: String },
    /// It was called beside a gated one and waits with it.
    Beside,
}

/// A card's approval state: what it waits on, and the controls.
#[derive(Clone, Copy, PartialEq)]
pub(super) struct Approval {
    pub hold: RwSignal<Option<Hold>>,
    /// The verdict picked on this card while several calls wait.
    pub pick: RwSignal<Option<bool>>,
    /// A decision is on its way: `Some(approve)`.
    pub sending: RwSignal<Option<bool>>,
    /// The refusal of the last decision, in words.
    pub said: RwSignal<Option<String>>,
}

impl Approval {
    pub fn new() -> Self {
        Self::held(None)
    }

    fn held(hold: Option<Hold>) -> Self {
        Self {
            hold: RwSignal::new(hold),
            pick: RwSignal::new(None),
            sending: RwSignal::new(None),
            said: RwSignal::new(None),
        }
    }
}

// ---------------------------------------------------------------------------
// Pure reading
// ---------------------------------------------------------------------------

/// How a call that did not run (or may not have) ended, read from its
/// result's words.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum Fate {
    /// Declined, with the reason when there was one.
    Declined(Option<String>),
    /// A new message came before it was decided.
    MovedOn,
    /// Not run, and why in the gateway's words.
    NotRun(String),
    /// Approved, but the turn that ran it ended before its result was
    /// saved.
    Unsure,
}

const DECLINED: &str = "The user declined this tool call";
const NOT_RUN: &str = "not run:";
const UNSURE: &str = "decided to run, but";

/// The fate a result text says, `None` for an ordinary result.
pub(super) fn fate_of(output: &str) -> Option<Fate> {
    let out = output.trim();
    if let Some(why) = out.strip_prefix(NOT_RUN) {
        return Some(if why.contains(MOVED_ON) {
            Fate::MovedOn
        } else {
            Fate::NotRun(why.trim().to_string())
        });
    }
    if let Some(rest) = out.strip_prefix(DECLINED) {
        let reason = rest
            .strip_prefix(':')
            .map(|r| r.trim().to_string())
            .filter(|r| !r.is_empty());
        return Some(match reason {
            Some(r) if r == MOVED_ON => Fate::MovedOn,
            r => Fate::Declined(r),
        });
    }
    out.starts_with(UNSURE).then_some(Fate::Unsure)
}

impl Fate {
    /// The summary's word.
    fn badge(&self) -> &'static str {
        match self {
            Self::Declined(_) => "declined",
            Self::MovedOn => "not decided",
            Self::NotRun(_) => "not run",
            Self::Unsure => "may not have run",
        }
    }

    /// The card's line, in words.
    fn line(&self) -> String {
        match self {
            Self::Declined(None) => "You declined this call; it did not run.".into(),
            Self::Declined(Some(r)) => format!("You declined this call ({r}); it did not run."),
            Self::MovedOn => {
                "Not decided: a new message came first, so this call never ran.".into()
            }
            Self::NotRun(why) => format!("Did not run: {why}."),
            Self::Unsure => "Approved, but the turn that ran it ended before its result was \
                saved, so it may or may not have run."
                .into(),
        }
    }
}

/// The inline words for a refused decision, by the route's code.
pub(super) fn refusal_words(code_: Option<&str>, message: &str) -> String {
    match code_ {
        Some(code::APPROVAL_MISSING) => "Not sent: another call on this reply still waits. \
            Decide every waiting call, then send."
            .into(),
        Some(code::APPROVAL_NOT_FOUND) => {
            "This call no longer waits for an approval. The conversation has been reloaded.".into()
        }
        Some(code::APPROVAL_DECIDED) => format!("Already decided: {message}."),
        Some(code::APPROVAL_MOVED_ON) => "This reply is no longer the last message, so its \
            calls cannot be resumed: a new message declined them, or the reply changed."
            .into(),
        Some(code::APPROVAL_STARTER_UNAVAILABLE) => {
            format!("The turn cannot run: the key that started it is gone or disabled. {message}")
        }
        Some(code::APPROVAL_OUT_OF_SCOPE) => format!("Not allowed: {message}"),
        _ => format!("The decision did not go through: {message}"),
    }
}

fn args_equal(shown: &str, sent: &str) -> bool {
    match (
        serde_json::from_str::<Value>(shown),
        serde_json::from_str::<Value>(sent),
    ) {
        (Ok(a), Ok(b)) => a == b,
        _ => shown.trim() == sent.trim(),
    }
}

/// Is this call (as the record or the stream names it) the request's? The
/// request has the tool's own name and its label; the record has the
/// exposed name, `<prefix>__name`.
fn is_call_of(name: &str, args: &str, req: &ApprovalRequest) -> bool {
    let exposed = format!("{}__{}", req.server_label, req.name);
    (name == exposed || name == req.name) && args_equal(args, &req.arguments)
}

/// The holds of a record's calls: each open request takes the first call
/// without a result that is its own, and every other call without a result
/// beside them is held with them. Nothing is held when nothing waits.
fn holds_for(tools: &[IrTool], pending: &[ApprovalRequest]) -> Vec<Option<Hold>> {
    let mut holds: Vec<Option<Hold>> = vec![None; tools.len()];
    if pending.is_empty() {
        return holds;
    }
    for req in pending {
        let at = tools.iter().enumerate().position(|(i, t)| {
            t.output.is_none() && holds[i].is_none() && is_call_of(&t.name, &t.args, req)
        });
        if let Some(i) = at {
            holds[i] = Some(Hold::Gated {
                id: req.approval_request_id.clone(),
                label: req.server_label.clone(),
            });
        }
    }
    for (i, t) in tools.iter().enumerate() {
        if t.output.is_none() && holds[i].is_none() {
            holds[i] = Some(Hold::Beside);
        }
    }
    holds
}

fn card_of(index: usize, t: IrTool, hold: Option<Hold>) -> ToolCard {
    ToolCard {
        index: index as i64,
        name: RwSignal::new(t.name),
        args: RwSignal::new(t.args),
        output: RwSignal::new(t.output.unwrap_or_default()),
        is_error: RwSignal::new(t.is_error),
        // Durations are not stored with the IR; the card shows "done"
        // instead of a made-up number.
        ms: RwSignal::new(None),
        done: RwSignal::new(true),
        approval: Approval::held(hold),
    }
}

/// The cards of a stored record, with the calls that wait.
pub(super) fn cards_of(ir: &str, pending: &[ApprovalRequest]) -> Vec<ToolCard> {
    let tools = tools_from_ir(ir);
    let holds = holds_for(&tools, pending);
    tools
        .into_iter()
        .zip(holds)
        .enumerate()
        .map(|(i, (t, h))| card_of(i, t, h))
        .collect()
}

/// Bring the cards shown to those of the stored record, in place (so an
/// open `<details>` stays open): a decision made elsewhere, or a resumed
/// turn that has ended, changes results and holds. Says whether anything
/// differed.
pub(super) fn merge_cards(shown: RwSignal<Vec<ToolCard>>, fresh: Vec<ToolCard>) -> bool {
    let mut changed = false;
    let mut added = Vec::new();
    shown.with_untracked(|shown| {
        for f in &fresh {
            let Some(c) = shown.iter().find(|c| c.index == f.index) else {
                added.push(*f);
                continue;
            };
            fn put<T: Clone + PartialEq + Send + Sync + 'static>(
                to: RwSignal<T>,
                from: RwSignal<T>,
                changed: &mut bool,
            ) {
                let v = from.get_untracked();
                if to.with_untracked(|t| *t != v) {
                    to.set(v);
                    *changed = true;
                }
            }
            put(c.name, f.name, &mut changed);
            put(c.args, f.args, &mut changed);
            put(c.output, f.output, &mut changed);
            put(c.is_error, f.is_error, &mut changed);
            put(c.done, f.done, &mut changed);
            let hold_changed = c.approval.hold.get_untracked() != f.approval.hold.get_untracked();
            put(c.approval.hold, f.approval.hold, &mut changed);
            if hold_changed {
                c.approval.pick.set(None);
            }
        }
    });
    if !added.is_empty() {
        shown.update(|s| s.extend(added));
        changed = true;
    }
    changed
}

// ---------------------------------------------------------------------------
// The stream
// ---------------------------------------------------------------------------

/// A `tool {event: "approval"}` frame: the card it is about (the first
/// without a result or a hold whose call is the request's) now waits.
pub(super) fn hold_for_frame(tools: RwSignal<Vec<ToolCard>>, v: &Value) {
    let req = ApprovalRequest {
        approval_request_id: v["approval_request_id"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
        server_label: v["server_label"].as_str().unwrap_or_default().to_string(),
        name: v["name"].as_str().unwrap_or_default().to_string(),
        arguments: v["arguments"].as_str().unwrap_or_default().to_string(),
        call_id: v["call_id"].as_str().map(str::to_string),
    };
    if req.approval_request_id.is_empty() {
        return;
    }
    tools.with_untracked(|tools| {
        let open = |c: &&ToolCard| {
            c.approval.hold.with_untracked(Option::is_none)
                && c.output.with_untracked(String::is_empty)
        };
        let hit = tools
            .iter()
            .filter(open)
            .find(|c| is_call_of(&c.name.get_untracked(), &c.args.get_untracked(), &req));
        // Arguments written differently by the model and the stream: the
        // name alone still finds the call.
        let hit = hit.or_else(|| {
            tools.iter().filter(open).find(|c| {
                let n = c.name.get_untracked();
                n == req.name || n == format!("{}__{}", req.server_label, req.name)
            })
        });
        if let Some(c) = hit {
            c.approval.hold.set(Some(Hold::Gated {
                id: req.approval_request_id.clone(),
                label: req.server_label.clone(),
            }));
            c.done.set(true);
        }
    });
}

/// `done` of a turn that stopped on calls: the calls without a result stop
/// "running"; those not gated wait beside the gated ones.
pub(super) fn settle_done(tools: RwSignal<Vec<ToolCard>>, done: &Value) {
    if done["pending_approvals"]
        .as_array()
        .is_none_or(Vec::is_empty)
    {
        return;
    }
    tools.with_untracked(|tools| {
        for c in tools.iter().filter(|c| !c.done.get_untracked()) {
            if c.approval.hold.with_untracked(Option::is_none) {
                c.approval.hold.set(Some(Hold::Beside));
            }
            c.done.set(true);
        }
    });
}

// ---------------------------------------------------------------------------
// Deciding
// ---------------------------------------------------------------------------

/// Verdicts for one reply's waiting calls: the message's key and
/// `(approval_request_id, approve)` for each.
#[derive(Clone)]
pub(super) struct Verdicts {
    pub key: u64,
    pub list: Vec<(String, bool)>,
}

fn waiting(tools: &[ToolCard]) -> Vec<(String, ToolCard)> {
    tools
        .iter()
        .filter_map(|c| match c.approval.hold.get() {
            Some(Hold::Gated { id, .. }) => Some((id, *c)),
            _ => None,
        })
        .collect()
}

impl ActionEnv {
    /// Decide the calls of the reply `v.key`, and stream the resumed turn
    /// into it.
    pub(super) fn decide(&self, v: Verdicts) {
        if self.busy() {
            return;
        }
        let Some((tid, _, _, target)) = self.locate(v.key) else {
            return;
        };
        let env = *self;
        let held = target.tools.with_untracked(|t| waiting(t));
        let cards: Vec<(ToolCard, bool)> = v
            .list
            .iter()
            .filter_map(|(id, approve)| {
                held.iter()
                    .find(|(h, _)| h == id)
                    .map(|(_, c)| (*c, *approve))
            })
            .collect();
        for (c, approve) in &cards {
            c.approval.sending.set(Some(*approve));
            c.approval.said.set(None);
        }
        let decisions: Vec<Value> = v
            .list
            .iter()
            .map(|(id, approve)| json!({ "approval_request_id": id, "approve": approve }))
            .collect();
        let turn = Turn {
            tid,
            url: format!("/chat/api/threads/{tid}/approvals"),
            body: json!({ "decisions": decisions }),
            target: target.clone(),
            continuing: true,
            what: "decide",
            inline: true,
        };
        let tools = target.tools;
        let accepted: Vec<ToolCard> = cards.iter().map(|(c, _)| *c).collect();
        spawn_local(async move {
            let end = run_turn(
                env.turn,
                turn,
                |_| {},
                move || {
                    // The server took the decisions: what was held runs
                    // (or is answered as declined) as the stream says.
                    tools.with_untracked(|all| {
                        for c in all.iter().filter(|c| {
                            accepted.contains(*c)
                                || c.approval.hold.get_untracked() == Some(Hold::Beside)
                        }) {
                            c.approval.hold.set(None);
                            c.approval.pick.set(None);
                            c.approval.sending.set(None);
                            c.done.set(false);
                        }
                    });
                },
            )
            .await;
            if let Some(r) = &end.refusal {
                let words = refusal_words(r.code.as_deref(), &r.message);
                for (c, _) in &cards {
                    c.approval.sending.set(None);
                    c.approval.said.set(Some(words.clone()));
                }
            }
            if env.turn.scope.alive() {
                // The stored reply is the truth, after a refusal as well
                // (someone else may have decided, or the reply moved on).
                env.resync(tid, true);
                env.refresh.run(());
            }
        });
    }
}

// ---------------------------------------------------------------------------
// Views
// ---------------------------------------------------------------------------

/// The summary's suffix and its class, when the card is in an approval
/// state.
fn status(card: ToolCard) -> Option<(&'static str, String)> {
    let a = card.approval;
    if let Some(approve) = a.sending.get() {
        return Some((
            "sending",
            if approve {
                "approving…"
            } else {
                "declining…"
            }
            .into(),
        ));
    }
    match a.hold.get() {
        Some(Hold::Gated { .. }) => return Some(("await", "waiting for approval".into())),
        Some(Hold::Beside) => return Some(("held", "held with a call that waits".into())),
        None => {}
    }
    let fate = card.output.with(|o| fate_of(o))?;
    let class = match fate {
        Fate::Declined(_) => "declined",
        Fate::MovedOn => "moved",
        Fate::NotRun(_) | Fate::Unsure => "notrun",
    };
    Some((class, fate.badge().into()))
}

/// One tool call: the card, and under it what the call waits on or how it
/// ended.
#[component]
pub(super) fn ToolCardView(
    card: ToolCard,
    tools: RwSignal<Vec<ToolCard>>,
    key: u64,
    ops: MsgOps,
) -> impl IntoView {
    let a = card.approval;
    // A call that waits shows its arguments: the card opens when it starts
    // to wait (it may be a card that was already showing, mid-stream).
    let details: NodeRef<leptos::html::Details> = NodeRef::new();
    Effect::new(move |_| {
        let gated = a.hold.with(|h| matches!(h, Some(Hold::Gated { .. })));
        if let Some(el) = details.get().filter(|_| gated) {
            el.set_open(true);
        }
    });
    let n_gated = Memo::new(move |_| waiting(&tools.get()).len());
    let verdict = move |approve: bool| {
        let Some(Hold::Gated { id, .. }) = a.hold.get_untracked() else {
            return;
        };
        if n_gated.get_untracked() <= 1 {
            ops.decide.run(Verdicts {
                key,
                list: vec![(id, approve)],
            });
        } else {
            a.pick.update(|p| {
                *p = if *p == Some(approve) {
                    None
                } else {
                    Some(approve)
                }
            });
        }
    };
    let locked = move || ops.busy.get() || a.sending.with(Option::is_some);
    view! {
        <details
            class="tool-card"
            class:err=move || {
                card.is_error.get() && a.hold.with(Option::is_none)
                    && card.output.with(|o| fate_of(o).is_none())
            }
            class=("await", move || a.hold.with(|h| matches!(h, Some(Hold::Gated { .. }))))
            node_ref=details
        >
            <summary>
                <span class="mono-sm">{move || card.name.get()}</span>
                {move || match status(card) {
                    Some((class, text)) => {
                        view! {
                            " · "
                            <span class=format!("tool-state {class}")>{text}</span>
                        }
                            .into_any()
                    }
                    None => {
                        view! {
                            {if card.done.get() {
                                // Replayed IR cards carry no duration — say "done"
                                // rather than nothing at all.
                                card.ms
                                    .get()
                                    .map(|ms| format!(" · {ms} ms"))
                                    .unwrap_or_else(|| " · done".to_string())
                            } else {
                                " · running…".to_string()
                            }}
                        }
                            .into_any()
                    }
                }}
            </summary>
            <div class="tool-io">
                <div class="dim mini-note">"arguments"</div>
                <pre class="preset">{move || card.args.get()}</pre>
                <Show when=move || !card.output.get().is_empty()>
                    <div class="dim mini-note">"output"</div>
                    <pre class="preset">{move || card.output.get()}</pre>
                </Show>
            </div>
        </details>
        {move || match a.hold.get() {
            Some(Hold::Gated { id, label }) => {
                let multi = n_gated.get() > 1;
                view! {
                    <div class="tool-ask" data-approval-request=id>
                        <span class="tool-ask-text">
                            "Waiting for your approval"
                            <span class="type-badge">{label}</span>
                        </span>
                        <span class="tool-ask-btns">
                            <button
                                type="button"
                                class="btn sm"
                                class:primary=move || !multi || a.pick.get() == Some(true)
                                class:on=move || multi && a.pick.get() == Some(true)
                                data-approve=""
                                disabled=locked
                                on:click=move |_| verdict(true)
                            >
                                "Approve"
                            </button>
                            <button
                                type="button"
                                class="btn sm"
                                class:on=move || multi && a.pick.get() == Some(false)
                                data-decline=""
                                disabled=locked
                                on:click=move |_| verdict(false)
                            >
                                "Decline"
                            </button>
                        </span>
                    </div>
                }
                    .into_any()
            }
            Some(Hold::Beside) => {
                view! {
                    <div class="tool-note" data-held="">
                        "Called beside a call that waits for an approval: it is decided with it."
                    </div>
                }
                    .into_any()
            }
            None => {
                card.output
                    .with(|o| fate_of(o))
                    .map(|f| view! { <div class="tool-note">{f.line()}</div> })
                    .into_any()
            }
        }}
        {move || {
            a.said
                .get()
                .map(|w| view! { <div class="tool-said" role="alert">{w}</div> })
        }}
    }
}

/// With several calls waiting, the verdicts picked on their cards are sent
/// together from here.
#[component]
pub(super) fn ApprovalBatch(
    tools: RwSignal<Vec<ToolCard>>,
    key: u64,
    ops: MsgOps,
) -> impl IntoView {
    let held = Memo::new(move |_| tools.with(|t| waiting(t)));
    let picks = move || {
        held.get()
            .into_iter()
            .map(|(id, c)| (id, c.approval.pick.get()))
            .collect::<Vec<_>>()
    };
    let all = move |approve: bool| {
        ops.decide.run(Verdicts {
            key,
            list: held
                .get_untracked()
                .into_iter()
                .map(|(id, _)| (id, approve))
                .collect(),
        });
    };
    let send = move |_| {
        let list: Option<Vec<(String, bool)>> = picks()
            .into_iter()
            .map(|(id, p)| p.map(|p| (id, p)))
            .collect();
        if let Some(list) = list {
            ops.decide.run(Verdicts { key, list });
        }
    };
    let busy = move || ops.busy.get();
    view! {
        <Show when=move || held.with(|h| h.len() > 1)>
            <div class="tool-batch" data-approval-batch="">
                <span class="tool-ask-text">
                    {move || format!("{} calls wait for a decision", held.with(Vec::len))}
                </span>
                <span class="tool-ask-btns">
                    <button
                        type="button"
                        class="btn sm"
                        disabled=busy
                        on:click=move |_| all(true)
                    >
                        "Approve all"
                    </button>
                    <button
                        type="button"
                        class="btn sm"
                        disabled=busy
                        on:click=move |_| all(false)
                    >
                        "Decline all"
                    </button>
                    <button
                        type="button"
                        class="btn sm primary"
                        disabled=move || busy() || picks().iter().any(|(_, p)| p.is_none())
                        on:click=send
                    >
                        {move || {
                            let p = picks();
                            let n = p.iter().filter(|(_, v)| v.is_some()).count();
                            format!("Send decisions ({n} of {})", p.len())
                        }}
                    </button>
                </span>
            </div>
        </Show>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(name: &str, args: &str, output: Option<&str>) -> IrTool {
        IrTool {
            name: name.into(),
            args: args.into(),
            output: output.map(str::to_string),
            is_error: false,
        }
    }

    fn req(id: &str, label: &str, name: &str, args: &str) -> ApprovalRequest {
        ApprovalRequest {
            approval_request_id: id.into(),
            server_label: label.into(),
            name: name.into(),
            arguments: args.into(),
            call_id: None,
        }
    }

    #[test]
    fn a_result_is_read_for_how_the_call_ended() {
        assert_eq!(fate_of("42"), None);
        assert_eq!(
            fate_of("The user declined this tool call."),
            Some(Fate::Declined(None))
        );
        assert_eq!(
            fate_of("The user declined this tool call: too risky"),
            Some(Fate::Declined(Some("too risky".into())))
        );
        // A new message declines with the gateway's own reason.
        assert_eq!(
            fate_of("The user declined this tool call: the user moved on without deciding"),
            Some(Fate::MovedOn)
        );
        assert_eq!(
            fate_of(
                "not run: it waited beside a call that needed an approval, and the user moved \
                 on without deciding"
            ),
            Some(Fate::MovedOn)
        );
        assert_eq!(
            fate_of("not run: the turn that was to run it once it was decided could not start"),
            Some(Fate::NotRun(
                "the turn that was to run it once it was decided could not start".into()
            ))
        );
        assert_eq!(
            fate_of("decided to run, but the turn that ran it ended before its result was saved"),
            Some(Fate::Unsure)
        );
    }

    #[test]
    fn refusals_are_worded_by_code() {
        let w = |c, m| refusal_words(Some(c), m);
        assert!(w("approval_missing", "x").starts_with("Not sent"));
        assert!(w("approval_not_found", "x").contains("no longer waits"));
        assert_eq!(
            w(
                "approval_decided",
                "call 'a' was already decided, by the dashboard"
            ),
            "Already decided: call 'a' was already decided, by the dashboard."
        );
        assert!(w("approval_moved_on", "x").contains("no longer the last message"));
        assert!(w("approval_starter_unavailable", "key 'k' is gone").contains("key 'k' is gone"));
        assert!(w("approval_out_of_scope", "phone may not").starts_with("Not allowed"));
        assert_eq!(
            refusal_words(None, "NetworkError"),
            "The decision did not go through: NetworkError"
        );
    }

    #[test]
    fn open_requests_take_their_calls_and_siblings_wait_beside() {
        let tools = vec![
            tool("docs__read", "{\"a\": 1}", Some("text")),
            tool("mail__send", "{\n  \"to\": \"x\"\n}", None),
            tool("docs__list", "{}", None),
        ];
        let pending = vec![req("mcpr_1", "mail", "send", "{\"to\":\"x\"}")];
        let holds = holds_for(&tools, &pending);
        assert_eq!(holds[0], None);
        assert_eq!(
            holds[1],
            Some(Hold::Gated {
                id: "mcpr_1".into(),
                label: "mail".into()
            })
        );
        assert_eq!(holds[2], Some(Hold::Beside));
    }

    #[test]
    fn nothing_waits_when_no_request_is_open() {
        let tools = vec![tool("a__b", "{}", None)];
        assert_eq!(holds_for(&tools, &[]), vec![None]);
    }

    #[test]
    fn two_calls_of_one_tool_pair_by_their_arguments() {
        let tools = vec![
            tool("m__send", "{\"to\": \"a\"}", None),
            tool("m__send", "{\"to\": \"b\"}", None),
        ];
        let pending = vec![
            req("r2", "m", "send", "{\"to\":\"b\"}"),
            req("r1", "m", "send", "{\"to\":\"a\"}"),
        ];
        let holds = holds_for(&tools, &pending);
        let id = |h: &Option<Hold>| match h {
            Some(Hold::Gated { id, .. }) => id.clone(),
            other => panic!("{other:?}"),
        };
        assert_eq!((id(&holds[0]), id(&holds[1])), ("r1".into(), "r2".into()));
    }
}
