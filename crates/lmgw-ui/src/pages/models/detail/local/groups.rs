//! The local model editor's fields as preference groups: General, Context,
//! GPU, Sampling, Reasoning, Files (with the ladder's rung table), Fallback,
//! and behind the Advanced row Speculative, the container, the flags, the
//! capability override and the command line as saved. Every field the
//! editor had is here; a group behind Advanced opens by itself when one of
//! its fields does not parse, and its row carries the change dot.

use leptos::prelude::*;
use lmgw_api_types::{LocalModelDetail, SettingsFull};

use super::cmdline::flag_lines;
use super::form::LocalForm;
use super::ladder::{enumerate_rungs, GgufBrowse, LadderRungRow};
use super::specs::{options, spec, Kind};
use crate::pages::fallback_field::{FallbackField, Saves};
use crate::widgets::{
    ActionRow, ComboRow, CopyBtn, EntryRow, Explain, ImageClass, ImagePicker, NumberRow, PrefGroup,
    Spin, StackRow, SwitchRow, TextRow,
};

/// One `LlamaParams` field as its row: a number, a choice or text, by its
/// spec, titled and explained by it. `step` puts − and + beside a number.
fn param(form: LocalForm, key: &'static str, step: Option<f64>) -> AnyView {
    let s = spec(key);
    let value = form.field(key);
    let dirty = form.is_dirty(key);
    let error = form.error_of(key);
    let wire = s.wire();
    match s.kind {
        Kind::Int | Kind::Float => {
            let spin = Spin {
                step: step.unwrap_or(1.0),
                min: None,
                max: None,
                digits: if s.kind == Kind::Float { 2 } else { 0 },
            };
            match wire {
                Some(w) => view! {
                    <NumberRow
                        title=s.label
                        sub=s.hint
                        wire=w
                        value=value
                        spin=spin
                        placeholder="default"
                        buttons=step.is_some()
                        dirty=dirty
                        error=error
                    />
                }
                .into_any(),
                None => view! {
                    <NumberRow
                        title=s.label
                        sub=s.hint
                        value=value
                        spin=spin
                        placeholder="default"
                        buttons=step.is_some()
                        dirty=dirty
                        error=error
                    />
                }
                .into_any(),
            }
        }
        Kind::Sel(_) => match wire {
            Some(w) => view! {
                <ComboRow title=s.label sub=s.hint wire=w value=value options=options(s.kind) dirty=dirty/>
            }
            .into_any(),
            None => view! {
                <ComboRow title=s.label sub=s.hint value=value options=options(s.kind) dirty=dirty/>
            }
            .into_any(),
        },
        Kind::Text => match wire {
            Some(w) => view! {
                <EntryRow
                    title=s.label
                    sub=s.hint
                    wire=w
                    value=value
                    placeholder="default"
                    mono=true
                    dirty=dirty
                    error=error
                />
            }
            .into_any(),
            None => view! {
                <EntryRow
                    title=s.label
                    sub=s.hint
                    value=value
                    placeholder="default"
                    mono=true
                    dirty=dirty
                    error=error
                />
            }
            .into_any(),
        },
    }
}

/// A file field across its row, with Browse.
fn file_row(
    title: &'static str,
    sub: &'static str,
    value: RwSignal<String>,
    role: &'static str,
    dirty: Signal<bool>,
    error: Option<Signal<Option<String>>>,
) -> AnyView {
    match error {
        Some(error) => view! {
            <EntryRow title=title sub=sub value=value mono=true wide=true dirty=dirty error=error>
                <GgufBrowse value=value role=role/>
            </EntryRow>
        }
        .into_any(),
        None => view! {
            <EntryRow title=title sub=sub value=value mono=true wide=true dirty=dirty>
                <GgufBrowse value=value role=role/>
            </EntryRow>
        }
        .into_any(),
    }
}

/// General, Context, GPU, Sampling, Reasoning and Files: the groups that
/// are always shown.
#[component]
pub(super) fn MainGroups(
    form: LocalForm,
    /// A new model: its files come right after General, they are what it
    /// is made from.
    create_mode: bool,
    /// The row as saved, for the context field's notice (device-ledger
    /// design §8.2); `None` before the first save.
    #[prop(into)]
    detail: Signal<Option<LocalModelDetail>>,
) -> impl IntoView {
    let f = form;
    view! {
        <PrefGroup title="General">
            <EntryRow
                title="Model id"
                sub="What it is served as, after the router's prefix"
                value=f.model_id
                mono=true
                dirty=f.is_dirty("model_id")
            />
            <SwitchRow title="Enabled" sub="Serves requests for this model" value=f.enabled dirty=f.is_dirty("enabled")/>
            <SwitchRow
                title="Listed"
                sub="Routable without an alias and in /v1/models; off, it is reached only through an alias"
                value=f.public
                dirty=f.is_dirty("public")
            />
            <SwitchRow
                title="Warm start"
                sub="Starts its container when lmgw starts"
                value=f.warm_start
                dirty=f.is_dirty("warm_start")
            />
            <NumberRow
                title="Unload when idle"
                sub="Seconds without a request before its container sleeps; 0 = never"
                value=f.idle_seconds
                spin=Spin { step: 60.0, ..Spin::default() }
                buttons=true
                dirty=f.is_dirty("idle_seconds")
                error=f.error_of("idle_seconds")
            />
        </PrefGroup>
        {create_mode.then(|| view! { <FilesGroup form=f/> })}
        <section class="pref-group">
            <div class="pref-head">
                <h2>"Context"</h2>
            </div>
            <Explain
                summary="Unified KV lets every parallel slot share one KV pool, so one conversation can use the whole context."
                persist="models.local.unified-kv"
            >
                "Without it a conversation is capped to ctx_size / parallel slots. llama-server aborts every running request when a shared pool overflows, which is why lmgw guards an explicitly unified row with a token ledger and needs Max output (n_predict) to bound it. Server default: unified exactly when Parallel slots is left blank."
            </Explain>
            <div class="boxed">
            {param(f, "ctx_size", Some(4096.0))}
            {move || {
                // While the field is empty: lmgw keeps the model on one card
                // of several (device-ledger design §8.2).
                let empty = f.field("ctx_size").get().trim().is_empty();
                detail
                    .get()
                    .and_then(|d| d.context_notice)
                    .filter(|_| empty)
                    .map(|n| view! { <div class="prow notice-row">{n}</div> })
            }}
            {param(f, "parallel", Some(1.0))}
            {param(f, "n_predict", Some(1024.0))}
            {param(f, "kv_unified", None)}
            {move || {
                f.pool_unguarded()
                    .then(|| {
                        view! {
                            <div class="prow notice-row warn">
                                "Slots share one pool unguarded: set Unified KV on with a Max output, or name the slots"
                            </div>
                        }
                    })
            }}
            {param(f, "kv_unified_per_slot", Some(4096.0))}
            {param(f, "batch_size", Some(256.0))}
            {param(f, "ubatch_size", Some(256.0))}
            </div>
        </section>
        <PrefGroup title="GPU">
            // The cards it may run on (device-ledger design §8).
            <StackRow
                title="Graphics card"
                sub=crate::pages::gpu_pin::PIN_HINT
                dirty=f.is_dirty("gpu_pin")
            >
                <crate::pages::gpu_pin::GpuPinChoice value=f.gpu_pin/>
            </StackRow>
            {param(f, "n_gpu_layers", Some(1.0))}
            {param(f, "n_cpu_moe", Some(1.0))}
            {param(f, "moe_cache_mib", Some(512.0))}
            {param(f, "flash_attn", None)}
            {param(f, "cache_type_k", None)}
            {param(f, "cache_type_v", None)}
            {param(f, "cache_ram", Some(1024.0))}
            {param(f, "fit", None)}
            {param(f, "fit_ctx", Some(4096.0))}
            {param(f, "threads", Some(1.0))}
        </PrefGroup>
        <PrefGroup title="Sampling" desc="The defaults a request's own fields override.">
            {param(f, "temp", Some(0.05))}
            {param(f, "top_p", Some(0.05))}
            {param(f, "top_k", Some(1.0))}
            {param(f, "min_p", Some(0.01))}
            {param(f, "repeat_penalty", Some(0.05))}
            {param(f, "presence_penalty", Some(0.1))}
            {param(f, "seed", Some(1.0))}
        </PrefGroup>
        <PrefGroup title="Reasoning" desc="And the chat template it is asked through.">
            {param(f, "reasoning", None)}
            {param(f, "reasoning_format", None)}
            {param(f, "reasoning_budget", Some(256.0))}
            {param(f, "reasoning_preserve", None)}
            {param(f, "reasoning_effort", None)}
            <SwitchRow
                title="Jinja"
                sub="Use the chat template embedded in the GGUF"
                value=f.jinja
                dirty=f.is_dirty("jinja")
            />
            <TextRow
                mono=true
                title="Template variables"
                sub="chat_template_kwargs, a JSON object; blank clears"
                value=f.kwargs
                dirty=f.is_dirty("chat_template_kwargs")
            />
        </PrefGroup>
        {(!create_mode).then(|| view! { <FilesGroup form=f/> })}
        <Show when=move || f.ladder_on.get()>
            <LadderGroup form=f/>
        </Show>
    }
}

/// The weights, the projector, the drafter and the template file.
#[component]
fn FilesGroup(form: LocalForm) -> impl IntoView {
    let f = form;
    view! {
        <PrefGroup title="Files" desc="Paths relative to the models dir.">
            // Rung 1 of the ladder table once Ladder is on (design §6): the
            // same field, shown in the other place.
            <Show when=move || !f.ladder_on.get()>
                {file_row("Weights", "The GGUF it serves", f.gguf_path, "weights", f.is_dirty("gguf_path"), None)}
            </Show>
            <SwitchRow
                title="Ladder"
                sub="A rung table instead of one GGUF: a request that does not fit climbs to a larger context"
                value=f.ladder_on
                dirty=f.is_dirty("ladder")
            />
            {file_row(
                "Projector (mmproj)",
                "The vision projector",
                f.field("mmproj_path"),
                "mmproj",
                f.is_dirty("mmproj_path"),
                Some(f.error_of("mmproj_path")),
            )}
            <SwitchRow
                title="Refuse an auto-loaded projector"
                sub="No projector unless one is named here"
                value=f.no_mmproj
                dirty=f.is_dirty("no_mmproj")
            />
            {file_row(
                "Draft model",
                "Speculative decoding weights",
                f.field("draft_gguf_path"),
                "draft",
                f.is_dirty("draft_gguf_path"),
                Some(f.error_of("draft_gguf_path")),
            )}
            {param(f, "chat_template_file", None)}
        </PrefGroup>
    }
}

/// The rung table (ladder design §4, §6).
#[component]
fn LadderGroup(form: LocalForm) -> impl IntoView {
    let f = form;
    view! {
        <PrefGroup
            title="Ladder"
            desc="One rung runs at a time, in this model's one container. A request that does not fit the running rung climbs straight to the smallest rung that fits, after the running rung has drained — every other conversation on this model moves up with it. It only comes back down when the container stops (the idle reaper, an eviction, the hold, an apply or restart); the next start is always the base rung."
        >
            <div class="prow stack ladder-box">
                <div class="table-scroll">
                    <table class="data ladder-table">
                        <thead>
                            <tr>
                                <th>"Rung"</th>
                                <th>"GGUF"</th>
                                <th>"Context"</th>
                                <th class="num-h">"Per slot"</th>
                                <th class="num-h">"Switchover"</th>
                                <th class="num-h">"Footprint"</th>
                                <th>"MTP"</th>
                                <th></th>
                            </tr>
                        </thead>
                        <tbody>
                            <LadderRungRow
                                rung_no=1
                                gguf_path=f.gguf_path
                                ctx_size=f.field("ctx_size")
                                shared=f.ladder_shared
                                error=f.error_of("gguf_path")
                            />
                            <For each=move || enumerate_rungs(f.rungs.get()) key=|(_, r)| r.key let:item>
                                {
                                    let (i, r) = item;
                                    let key = r.key;
                                    view! {
                                        <LadderRungRow
                                            rung_no=i + 2
                                            gguf_path=r.gguf_path
                                            ctx_size=r.ctx_size
                                            shared=f.ladder_shared
                                            error=f.rung_error_of(key)
                                            on_remove=Callback::new(move |()| {
                                                f.rungs.update(|rows| rows.retain(|x| x.key != key));
                                            })
                                        />
                                    }
                                }
                            </For>
                        </tbody>
                    </table>
                </div>
                <div class="row">
                    <button type="button" class="btn sm" on:click=move |_| f.add_rung()>
                        "Add rung"
                    </button>
                </div>
                {move || f.ladder_table_error.get().map(|m| view! { <div class="prow-err" role="alert">{m}</div> })}
            </div>
            <NumberRow
                title="Max output"
                sub="Required on a ladder: every request is clamped to it"
                wire="--n-predict"
                value=f.field("n_predict")
                placeholder="required"
                dirty=f.is_dirty("n_predict")
                error=f.error_of("n_predict")
            />
        </PrefGroup>
    }
}

/// What answers when this model can't (openai egress design §14).
#[component]
pub(super) fn FallbackGroup(
    form: LocalForm,
    saves: Saves,
    #[prop(into)] own_name: Signal<String>,
) -> impl IntoView {
    view! {
        <PrefGroup title="Fallback">
            <div class="prow stack field-row" class:dirty=move || form.is_dirty("fallback").get()>
                <FallbackField
                    value=form.fallback
                    tasks=Signal::derive(|| &["chat"][..])
                    own_name=own_name
                    saves=saves
                    dirty=form.is_dirty("fallback")
                    hint="Answers whenever this model can't: while the GPU is held, when VRAM outside lmgw is short, for background requests that would disturb other work, and while the row is disabled. A chat model: a plain alias, another local model or a passthrough id."
                />
            </div>
        </PrefGroup>
    }
}

/// The Advanced row and, while it is open, the groups behind it.
#[component]
pub(super) fn AdvancedGroups(
    form: LocalForm,
    class_settings: Signal<Option<SettingsFull>>,
    /// The row as saved, for its command line; `None` before the first save.
    #[prop(into)]
    detail: Signal<Option<LocalModelDetail>>,
    create_mode: bool,
) -> impl IntoView {
    let f = form;
    let open = crate::prefs::persisted_bool("open.models.local.advanced", false);
    const HIDDEN: &[&str] = &["Speculative", "Advanced"];
    let dirty = f.groups_dirty(HIDDEN);
    let invalid = f.groups_invalid(HIDDEN);
    // A field behind the row that does not parse is not left out of sight.
    Effect::new(move |_| {
        if invalid.get() && !open.get_untracked() {
            open.set(true);
        }
    });
    let shown = move || open.get();
    view! {
        <PrefGroup>
            <ActionRow
                title="Advanced"
                sub="Speculative decoding, the container, extra flags, the capability override and the command line"
                dirty=dirty
                chevron=true
                on_click=Callback::new(move |()| open.update(|o| *o = !*o))
            />
        </PrefGroup>
        <Show when=shown>
            <PrefGroup title="Speculative">
                {param(f, "spec_type", None)}
                {param(f, "spec_draft_n_max", Some(1.0))}
                {param(f, "spec_draft_n_min", Some(1.0))}
                {param(f, "spec_draft_ngl", None)}
            </PrefGroup>
            <PrefGroup
                title="Container"
                desc="Blank fields inherit the chat class settings. An override replaces the class args whole: to run without some of them, list the ones to keep."
            >
                <div class="prow stack field-row" class:dirty=move || f.is_dirty("image").get()>
                    <div class="prow-text">
                        <span class="prow-title">"Image override"</span>
                        <span class="prow-sub">"The llama-server image this model runs in"</span>
                    </div>
                    <div class="prow-end">
                        <ImagePicker
                            value=f.image
                            class=ImageClass::Chat
                            clearable=true
                            placeholder=Signal::derive(move || {
                                class_settings.get().map(|s| s.router.image).unwrap_or_default()
                            })
                        />
                    </div>
                </div>
                <TextRow
                    mono=true
                    title="Extra podman run args"
                    sub="One argument per line"
                    value=f.extra_run_args
                    placeholder=move || {
                        class_settings.get().map(|s| s.router.extra_run_args.join("\n")).unwrap_or_default()
                    }
                    dirty=f.is_dirty("extra_run_args")
                    extra=move || {
                        view! {
                            "The class's own: "
                            <a href=crate::pages::settings::href("router.extra_run_args")>
                                "Settings → Runtimes → Chat"
                            </a>
                        }
                    }
                />
            </PrefGroup>
            <PrefGroup title="Flags">
                <TextRow
                    mono=true
                    title="Extra llama-server flags"
                    sub="One argument per line"
                    value=f.extra_args
                    dirty=f.is_dirty("extra_args")
                />
            </PrefGroup>
            <PrefGroup title="Capability override">
                <TextRow
                    mono=true
                    title="Capability override"
                    sub="A JSON object merged over what lmgw derives for /v1/models — optional keys capabilities (deep-merged), max_output_tokens, notes (appended). Only for facts the GGUF does not state; published with source \"owner\". Blank clears."
                    value=f.capabilities_override
                    dirty=f.is_dirty("capabilities_override")
                />
            </PrefGroup>
            <PrefGroup
                title="Command line"
                desc=move || {
                    if create_mode {
                        "Rendered once the model is saved.".to_string()
                    } else if f.dirty.with(|d| !d.is_empty()) {
                        "As saved: unsaved edits are not in it yet.".to_string()
                    } else {
                        "What this model's own container is started with; the published port is allocated per start.".to_string()
                    }
                }
                suffix=move || {
                    detail
                        .get()
                        .map(|d| view! { <CopyBtn text=d.command_line title="Copy the command as one line"/> })
                }
            >
                {move || match detail.get() {
                    Some(d) => {
                        let note = d
                            .command_line_note
                            .clone()
                            .map(|n| view! { <div class="prow notice-row">{n}</div> });
                        view! { <pre class="cmd-lines">{flag_lines(&d.command_line)}</pre> {note} }
                            .into_any()
                    }
                    None if create_mode => view! { <div class="prow dim">"Not saved yet"</div> }.into_any(),
                    None => view! { <div class="prow dim">"Loading…"</div> }.into_any(),
                }}
            </PrefGroup>
        </Show>
    }
}
