//! The component sample (a debug build only, `/dev/components`): every
//! shared widget of the modern design on one page, so a change to one can be
//! seen in both themes without finding a page that uses it. Nothing here
//! talks to the gateway.

use leptos::prelude::*;

use crate::widgets::list_pane::{GroupHead, ListBox, ListHead, ListNav, ListRefs, ListSearch};
use crate::widgets::{
    use_toasts, ActionRow, AudioPlayer, ComboRow, Dot, EntryRow, Ico, Icon, ImageClass,
    ImagePicker, InfoRow, ListRow, MenuItem, Modal, ModalFooter, ModelPicker, PageFrame, PageMode,
    PrefGroup, RowMenu, SaveToast, Select, Spin, SpinRow, StateChip, Switch, SwitchRow, WireName,
};

/// A short quiet sine as an object URL: the audio player's sample needs no
/// model and no network.
fn sample_clip_url() -> Option<String> {
    let rate = 8000_u32;
    let pcm: Vec<u8> = (0..rate * 3)
        .flat_map(|i| {
            let t = i as f32 / rate as f32;
            let v = (t * 330.0 * std::f32::consts::TAU).sin() * 0.08;
            ((v * f32::from(i16::MAX)) as i16).to_le_bytes()
        })
        .collect();
    let wav = lmgw_ui_kit::wav::pcm_to_wav(&pcm, rate, 1, 16);
    let parts = js_sys::Array::new();
    parts.push(&js_sys::Uint8Array::from(wav.as_slice()));
    let opts = web_sys::BlobPropertyBag::new();
    opts.set_type("audio/wav");
    web_sys::Blob::new_with_u8_array_sequence_and_options(&parts, &opts)
        .ok()
        .and_then(|b| web_sys::Url::create_object_url_with_blob(&b).ok())
}

fn sample_menu() -> Vec<MenuItem> {
    vec![
        MenuItem::new("Open", || {}),
        MenuItem::new("Duplicate", || {}),
        MenuItem::new("Export…", || {}).separated(),
        MenuItem::new("Delete…", || {}).separated().destructive(),
    ]
}

/// One row of the list pane sample.
#[derive(Clone)]
struct PaneRow {
    group: &'static str,
    name: &'static str,
    meta: &'static str,
    dot: Dot,
    state: &'static str,
    state_ico: Option<(Ico, &'static str)>,
    quiet: bool,
}

fn pane_rows() -> Vec<PaneRow> {
    let row = |group, name, meta, dot, state, state_ico, quiet| PaneRow {
        group,
        name,
        meta,
        dot,
        state,
        state_ico,
        quiet,
    };
    vec![
        row(
            "Models",
            "qwen3.8-flash-next",
            "Q4_K_M · 18.6 GiB",
            Dot::Running,
            "Running",
            None,
            false,
        ),
        row(
            "Models",
            "gemma-4-12b",
            "Q6_K · 9.8 GiB",
            Dot::Busy,
            "Starting",
            None,
            false,
        ),
        row(
            "Models",
            "gemma-4-27b",
            "UD-Q4_K_XL · 16.4 GiB",
            Dot::Stopped,
            "Stopped",
            None,
            false,
        ),
        row(
            "Models",
            "smollm-3b",
            "Q8_0 · 3.2 GiB",
            Dot::Off,
            "Disabled",
            None,
            false,
        ),
        row(
            "Aliases",
            "fast-chat",
            "Two candidates",
            Dot::Remote,
            "Ready",
            Some((Ico::Cloud, "Served by an upstream")),
            false,
        ),
        row(
            "Aliases",
            "No model",
            "Nothing answers it yet",
            Dot::Warn,
            "Needs a look",
            Some((Ico::Info, "Nothing answers this alias yet")),
            true,
        ),
    ]
}

#[component]
pub fn Components() -> impl IntoView {
    let enabled = RwSignal::new(true);
    let listed = RwSignal::new(true);
    let warm = RwSignal::new(false);
    let ctx = RwSignal::new(131_072.0);
    let slots = RwSignal::new(1.0);
    let fa = RwSignal::new("auto".to_string());
    let pick = RwSignal::new("q8_0".to_string());
    let check = RwSignal::new(true);
    let sel = RwSignal::new(0usize);
    let seg = RwSignal::new(0usize);
    let toasts = use_toasts();
    // The sample tone is an object URL: handed back when the page goes.
    let clip = sample_clip_url();
    let no_clip: Option<String> = None;
    let revoke = clip.clone();
    on_cleanup(move || {
        if let Some(u) = &revoke {
            let _ = web_sys::Url::revoke_object_url(u);
        }
    });
    let dialog_open = RwSignal::new(false);
    let dialog_name = RwSignal::new(String::new());
    let changed = RwSignal::new(false);
    let pane_query = RwSignal::new(String::new());
    let pane_sel = RwSignal::new(Some("gemma-4-12b".to_string()));
    let refs = ListRefs::<String>::new();
    let pane_rows = pane_rows();
    let order = {
        let pane_rows = pane_rows.clone();
        Signal::derive(move || {
            let q = pane_query.get().to_lowercase();
            pane_rows
                .iter()
                .filter(|r| r.name.to_lowercase().contains(&q))
                .map(|r| r.name.to_string())
                .collect::<Vec<_>>()
        })
    };
    let pane_nav = ListNav::new(
        refs,
        order,
        pane_sel,
        Memo::new(|_| false),
        Callback::new(move |(id, _): (String, bool)| pane_sel.set(Some(id))),
        String::clone,
    );
    let pane_groups = {
        let pane_rows = pane_rows.clone();
        move || {
            let shown = order.get();
            ["Models", "Aliases"]
                .into_iter()
                .filter_map(|g| {
                    let rows: Vec<PaneRow> = pane_rows
                        .iter()
                        .filter(|r| r.group == g && shown.iter().any(|n| n == r.name))
                        .cloned()
                        .collect();
                    (!rows.is_empty()).then_some((g, rows))
                })
                .collect::<Vec<_>>()
        }
    };
    let model_pick = RwSignal::new(String::new());
    let image_pick = RwSignal::new(String::new());
    // What the rows started at: the dirty dots and the toast count from it.
    let base = (true, true, false, 131_072.0, 1.0, "auto".to_string());
    let dirty_enabled = Signal::derive(move || enabled.get() != base.0);
    let dirty_listed = Signal::derive(move || listed.get() != base.1);
    let dirty_warm = Signal::derive(move || warm.get() != base.2);
    let dirty_ctx = Signal::derive(move || ctx.get() != base.3);
    let dirty_slots = Signal::derive(move || slots.get() != base.4);
    let base_fa = base.5.clone();
    let dirty_fa = Signal::derive(move || fa.with(|v| *v != base_fa));
    let dirty_count = Signal::derive(move || {
        [
            dirty_enabled.get(),
            dirty_listed.get(),
            dirty_warm.get(),
            dirty_ctx.get(),
            dirty_slots.get(),
            dirty_fa.get(),
            changed.get(),
        ]
        .iter()
        .filter(|d| **d)
        .count()
    });
    // Typed text the spins refuse: Save waits for it (the toast's count).
    let ctx_bad = RwSignal::new(None::<String>);
    let slots_bad = RwSignal::new(None::<String>);
    let invalid = Signal::derive(move || {
        [ctx_bad, slots_bad]
            .iter()
            .filter(|b| b.with(Option::is_some))
            .count()
    });
    let discard = Callback::new(move |()| {
        enabled.set(true);
        listed.set(true);
        warm.set(false);
        ctx.set(131_072.0);
        slots.set(1.0);
        fa.set("auto".into());
        changed.set(false);
    });
    let opts = |list: &[(&str, &str)]| {
        let v: Vec<(String, String)> = list
            .iter()
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .collect();
        Signal::derive(move || v.clone())
    };
    let fa_opts = opts(&[("auto", "Auto"), ("on", "On"), ("off", "Off")]);
    let kv_opts = opts(&[("f16", "f16"), ("q8_0", "q8_0"), ("q4_0", "q4_0")]);
    let rows: [(&str, &str, Dot, &str); 7] = [
        (
            "qwen3.8-flash-next",
            "Q4_K_M · 18.6 GiB · 131 072 context",
            Dot::Running,
            "Running",
        ),
        (
            "gemma-4-12b",
            "Q6_K · 9.8 GiB · 65 536 context",
            Dot::Busy,
            "Starting",
        ),
        (
            "gemma-4-27b",
            "UD-Q4_K_XL · 16.4 GiB · 32 768 context",
            Dot::Stopped,
            "Stopped",
        ),
        (
            "cloud/example-model",
            "Example upstream · example-model",
            Dot::Remote,
            "Ready",
        ),
        (
            "smollm-3b",
            "Q8_0 · 3.2 GiB · switched off",
            Dot::Off,
            "Disabled",
        ),
        (
            "mail-labeler",
            "Batch agent · one warning",
            Dot::Warn,
            "Needs a look",
        ),
        (
            "nightly-bench",
            "Benchmark run · phase 2 of 4",
            Dot::Busy,
            "Running 48%",
        ),
    ];
    view! {
        <PageFrame
            title="Components"
            sub="The shared widgets of the modern design"
            mode=PageMode::Doc
            actions=move || {
                view! {
                    <button class="btn flat">
                        <Icon ico=Ico::Play/>
                        "Start"
                    </button>
                    <button class="btn">"Test"</button>
                    <button class="btn suggested">"Save"</button>
                    <button class="icon-btn" title="More">
                        <Icon ico=Ico::More/>
                    </button>
                }
            }
        >
            <div class="detail-scroll comp-sample">
                <div class="detail-flow">
                    <div class="detail-hero">
                        <div class="glyph">
                            <Icon ico=Ico::Download/>
                        </div>
                        <div style="min-width:0">
                            <h1>"qwen3.8-flash-next"</h1>
                            <div class="sub">"Qwen3.8-Flash-Next-Q4_K_M.gguf"</div>
                            <div class="chips">
                                <span class="chip ok">
                                    <span class="dot"></span>
                                    "Running · 18.6 GiB VRAM"
                                </span>
                                <span class="chip">"Vision"</span>
                                <span class="chip">"Tools"</span>
                                <span class="chip warn">"Hold"</span>
                                <span class="chip err">"Error"</span>
                                <span class="chip info">
                                    <span class="dot"></span>
                                    "downloading"
                                </span>
                            </div>
                        </div>
                    </div>
                    <PrefGroup title="General">
                        <SwitchRow
                            title="Enabled"
                            sub="Serves requests for this model"
                            value=enabled
                            dirty=dirty_enabled
                        />
                        <SwitchRow
                            title="Listed"
                            sub="Shows up in /v1/models"
                            value=listed
                            dirty=dirty_listed
                        />
                        <SwitchRow
                            title="Warm start"
                            sub="Loads when lmgw starts"
                            value=warm
                            dirty=dirty_warm
                        />
                    </PrefGroup>
                    <PrefGroup
                        title="Context"
                        desc="The largest context that fits next to the weights is filled in when the model is planned."
                    >
                        <SpinRow
                            title="Context size"
                            sub="Trained for 262 144 tokens"
                            value=ctx
                            spin=Spin {
                                step: 4096.0,
                                min: Some(0.0),
                                max: Some(262_144.0),
                                digits: 0,
                            }
                            unit="tokens"
                            dirty=dirty_ctx
                            invalid=ctx_bad
                        />
                        <SpinRow
                            title="Parallel slots"
                            sub="Each slot gets the full context"
                            value=slots
                            spin=Spin {
                                step: 1.0,
                                min: Some(1.0),
                                max: None,
                                digits: 0,
                            }
                            dirty=dirty_slots
                            invalid=slots_bad
                        />
                    </PrefGroup>
                    <PrefGroup title="GPU">
                        <ComboRow title="Flash attention" value=fa options=fa_opts dirty=dirty_fa/>
                        <ComboRow
                            title="K cache type"
                            sub="Smaller types fit more context"
                            value=pick
                            options=kv_opts
                        />
                    </PrefGroup>
                    <PrefGroup title="File">
                        <InfoRow title="Path" value="models/Qwen3.8-Flash-Next-Q4_K_M.gguf" mono=true/>
                        <InfoRow title="Size" value="18.6 GiB"/>
                        <ActionRow title="Show in folder" chevron=true on_click=Callback::new(|()| {})/>
                    </PrefGroup>
                    <PrefGroup>
                        <ActionRow
                            title="Reset to defaults"
                            sub="Off while nothing differs from them"
                            disabled=true
                            on_click=Callback::new(|()| {})
                        />
                        <ActionRow
                            title="Delete model…"
                            sub="Removes the row; the file stays on disk"
                            danger=true
                            on_click=Callback::new(|()| {})
                        />
                    </PrefGroup>
                    <PrefGroup title="List rows" desc="The two-line row with its state dot.">
                        <div class="comp-list" tabindex="0">
                            {rows
                                .into_iter()
                                .enumerate()
                                .map(|(i, (name, meta, dot, state))| {
                                    view! {
                                        <ListRow
                                            name=name
                                            meta=meta
                                            dot=Signal::stored(dot)
                                            state=state
                                            selected=Signal::derive(move || sel.get() == i)
                                            off=Signal::stored(dot == Dot::Off)
                                            on_click=Callback::new(move |()| sel.set(i))
                                        />
                                    }
                                })
                                .collect_view()}
                        </div>
                    </PrefGroup>
                    <PrefGroup
                        title="Menu buttons"
                        desc="A label with its chevron, one with a leading icon too, a label alone, a round icon button, the kebab, and a menu with a rule and a destructive item."
                    >
                        <div class="comp-controls">
                            <div class="row">
                                <RowMenu
                                    items=sample_menu()
                                    title="A label and a chevron"
                                    label=Signal::derive(|| "Examples".to_string())
                                    chevron=true
                                />
                                <RowMenu
                                    items=sample_menu()
                                    title="A label"
                                    label=Signal::derive(|| "Load".to_string())
                                />
                                <RowMenu
                                    items=sample_menu()
                                    title="An icon, a label and a chevron"
                                    label=Signal::derive(|| "Add".to_string())
                                    icon=Ico::Plus
                                    chevron=true
                                />
                                <RowMenu items=sample_menu() title="An icon" icon=Ico::Plus/>
                                <RowMenu items=sample_menu()/>
                            </div>
                        </div>
                    </PrefGroup>
                    <PrefGroup
                        title="Audio"
                        desc="The kit's player row on a three-second clip the page makes itself; the second has no clip yet and the third cannot be loaded."
                    >
                        <div class="comp-controls">
                            <AudioPlayer src=clip.clone() label="A sample tone" preload="metadata"/>
                            <AudioPlayer src=no_clip/>
                            <AudioPlayer src=Some("/dev/no-such-clip.wav".to_string())/>
                        </div>
                    </PrefGroup>
                    <PrefGroup title="Dialog" desc="Opened with the mouse, the dialog holds the focus itself: no ring on its close button until Tab.">
                        <div class="comp-controls">
                            <div class="row">
                                <button class="btn" on:click=move |_| dialog_open.set(true)>
                                    "Open a dialog"
                                </button>
                            </div>
                        </div>
                    </PrefGroup>
                    <Modal open=dialog_open title="A dialog">
                        <PrefGroup>
                            <EntryRow
                                title="Name"
                                sub="Nothing is saved from here"
                                value=dialog_name
                                placeholder="A name"
                            />
                        </PrefGroup>
                        <ModalFooter>
                            <button class="btn" on:click=move |_| dialog_open.set(false)>
                                "Cancel"
                            </button>
                            <button class="btn suggested" on:click=move |_| dialog_open.set(false)>
                                "Done"
                            </button>
                        </ModalFooter>
                    </Modal>
                    <PrefGroup title="Toasts" desc="The three kinds, and the save bar that a change raises.">
                        <div class="comp-controls">
                            <div class="row">
                                <button class="btn" on:click=move |_| toasts.ok("Something worked.")>
                                    "Ok toast"
                                </button>
                                <button class="btn" on:click=move |_| toasts.warn("Something needs a look.")>
                                    "Warn toast"
                                </button>
                                <button class="btn" on:click=move |_| toasts.err("Something failed.")>
                                    "Error toast"
                                </button>
                                <Switch value=changed label="Make a change"/>
                            </div>
                        </div>
                    </PrefGroup>
                    <PrefGroup title="List pane" desc="The header, the search, the groups with their counts and the rows with every dot; the search filters here.">
                        <div class="comp-pane">
                            <ListHead
                                scrolled=refs.scrolled
                                narrow=Memo::new(|_| false)
                                title="Models"
                                sub=Signal::derive(move || format!("{} shown", order.get().len()))
                            />
                            <ListSearch nav=pane_nav query=pane_query placeholder="Search models"/>
                            <ListBox nav=pane_nav label="Models">
                                {move || {
                                    pane_groups()
                                        .into_iter()
                                        .map(|(g, rows)| {
                                            let n = rows.len();
                                            view! {
                                                <GroupHead label=g n=Signal::stored(n)/>
                                                {rows
                                                    .into_iter()
                                                    .map(|r| {
                                                        let id = r.name.to_string();
                                                        let sel_id = id.clone();
                                                        view! {
                                                            <ListRow
                                                                name=r.name
                                                                meta=r.meta
                                                                dot=Signal::stored(r.dot)
                                                                state=r.state
                                                                state_ico=r.state_ico.map(|(i, _)| i)
                                                                state_title=r.state_ico.map(|(_, t)| t).unwrap_or_default()
                                                                quiet_name=r.quiet
                                                                key=id.clone()
                                                                off=Signal::stored(r.dot == Dot::Off)
                                                                selected=Signal::derive(move || {
                                                                    pane_sel.with(|s| s.as_deref() == Some(sel_id.as_str()))
                                                                })
                                                                on_click=Callback::new(move |()| pane_sel.set(Some(id.clone())))
                                                            />
                                                        }
                                                    })
                                                    .collect_view()}
                                            }
                                        })
                                        .collect_view()
                                }}
                            </ListBox>
                        </div>
                    </PrefGroup>
                    <PrefGroup title="Status page" desc="What a detail shows when nothing is open: a hero, then link rows.">
                        <div class="comp-controls">
                            <div class="detail-hero">
                                <div class="glyph">
                                    <Icon ico=Ico::Search/>
                                </div>
                                <div style="min-width:0">
                                    <h1>"Nothing lives at this URL"</h1>
                                    <div class="sub">"It may be an old link: these moved."</div>
                                </div>
                            </div>
                        </div>
                    </PrefGroup>
                    <PrefGroup title="In the list">
                        {["Models", "Downloads", "Traffic"]
                            .into_iter()
                            .map(|t| {
                                view! {
                                    <a
                                        class="prow action"
                                        href="#"
                                        on:click=move |ev| ev.prevent_default()
                                    >
                                        <div class="prow-text">
                                            <span class="prow-title">{t}</span>
                                            <span class="prow-sub">"A line saying what is there"</span>
                                        </div>
                                        <Icon ico=Ico::Right class="prow-chevron"/>
                                    </a>
                                }
                            })
                            .collect_view()}
                    </PrefGroup>
                    <PrefGroup title="Pickers" desc="Both read the lists only; nothing is written. The image picker lists local images through podman: where that cannot be read it says so and still takes a typed reference.">
                        <div class="comp-controls">
                            <ModelPicker value=model_pick tasks=&["chat"]/>
                            <ImagePicker value=image_pick class=ImageClass::Chat clearable=true/>
                        </div>
                    </PrefGroup>
                    <PrefGroup title="Long values" desc="A long value wraps in its row or ends in an ellipsis; its tooltip holds the whole.">
                        <InfoRow
                            title="Description"
                            value="A sentence far longer than a row holds, about 200 characters in all, so that the value has to wrap or be cut instead of pushing the row wider than the box it sits in. It ends here."
                        />
                        <InfoRow
                            title="Path"
                            value="/home/example/.local/share/lmgw/models/a-very-long-directory-name/another-long-directory-name/Qwen3.8-Flash-Next-Q4_K_M-00001-of-00004.gguf"
                            mono=true
                        />
                    </PrefGroup>
                    <PrefGroup title="Record states" desc="The chip a hero shows, one per dot.">
                        <div class="comp-controls">
                            <div class="row">
                                {[
                                    (Dot::Running, "Running"),
                                    (Dot::Warn, "Needs a look"),
                                    (Dot::Busy, "In progress"),
                                    (Dot::Stopped, "Stopped"),
                                    (Dot::Remote, "Ready"),
                                    (Dot::Error, "Failed"),
                                    (Dot::Off, "Disabled"),
                                ]
                                    .into_iter()
                                    .map(|(d, w)| view! { <StateChip dot=Signal::stored(d) word=w/> })
                                    .collect_view()}
                            </div>
                        </div>
                    </PrefGroup>
                    <PrefGroup title="With a count" count=Signal::stored("3".to_string())>
                        <InfoRow title="First" value="One"/>
                        <InfoRow title="Second" value="Two"/>
                        <InfoRow title="Third" value="Three"/>
                    </PrefGroup>
                    <PrefGroup title="Wire names" desc="Whole on a line of their own under the title; a narrow row ends one in an ellipsis.">
                        <div class="prow" style="max-width:340px">
                            <div class="prow-text">
                                <span class="prow-title">
                                    "Sampling steps"
                                    <WireName name="sample_params.sample_steps"/>
                                </span>
                            </div>
                        </div>
                        <div class="prow" style="max-width:200px">
                            <div class="prow-text">
                                <span class="prow-title">
                                    "Guidance"
                                    <WireName name="sample_params.guidance.txt_cfg_and_a_much_longer_tail"/>
                                </span>
                            </div>
                        </div>
                    </PrefGroup>
                    <PrefGroup title="Icons" desc="Every icon of the kit, its name as the tooltip.">
                        <div class="comp-controls">
                            <div class="comp-icons">
                                {Ico::ALL
                                    .iter()
                                    .map(|i| {
                                        view! {
                                            <span class="comp-ico" title=format!("{i:?}")>
                                                <Icon ico=*i/>
                                            </span>
                                        }
                                    })
                                    .collect_view()}
                            </div>
                        </div>
                    </PrefGroup>
                    <PrefGroup title="Controls">
                        <div class="comp-controls">
                            <div class="row">
                                <button class="btn">"Default"</button>
                                <button class="btn flat">"Flat"</button>
                                <button class="btn suggested">"Suggested"</button>
                                <button class="btn destructive">"Destructive"</button>
                                <button class="btn" disabled=true>"Disabled"</button>
                                <button class="btn sm">"Small"</button>
                                <button class="icon-btn" title="Add">
                                    <Icon ico=Ico::Plus/>
                                </button>
                            </div>
                            <div class="row">
                                <input class="input" placeholder="A text field"/>
                                <Select value=pick options=kv_opts/>
                                <Switch value=check label="A switch"/>
                                <label class="row" style="gap:6px">
                                    <input type="checkbox" checked=true/>
                                    "A checkbox"
                                </label>
                            </div>
                            <div class="row">
                                <div class="seg">
                                    {["Form", "Results", "History"]
                                        .into_iter()
                                        .enumerate()
                                        .map(|(i, l)| {
                                            view! {
                                                <button
                                                    class="seg-btn"
                                                    class:active=move || seg.get() == i
                                                    on:click=move |_| seg.set(i)
                                                >
                                                    {l}
                                                    {(i == 1).then(|| view! { <span class="count">"3"</span> })}
                                                </button>
                                            }
                                        })
                                        .collect_view()}
                                </div>
                                <span class="type-badge">"chat"</span>
                                <span class="count">"12"</span>
                                <span class="count attn">"2"</span>
                                <span class="count bad">"1"</span>
                            </div>
                        </div>
                    </PrefGroup>
                    <PrefGroup title="Notices">
                        <div class="comp-controls">
                            <div class="notice">
                                <b>"A plain notice"</b>
                                "What a page says in passing."
                            </div>
                            <div class="notice warn">
                                <b>"A warning"</b>
                                "Something the owner should know."
                            </div>
                            <div class="notice err">"An error, in its own words."</div>
                            <div class="empty">"Nothing to show yet"</div>
                        </div>
                    </PrefGroup>
                    <PrefGroup title="Table">
                        <table class="data">
                            <thead>
                                <tr>
                                    <th>"Model"</th>
                                    <th>"State"</th>
                                    <th class="num-h">"Size"</th>
                                    <th></th>
                                </tr>
                            </thead>
                            <tbody>
                                <tr>
                                    <td>"qwen3.8-flash-next"</td>
                                    <td>
                                        <span class="chip ok">
                                            <span class="dot"></span>
                                            "running"
                                        </span>
                                    </td>
                                    <td class="num">"18.6 GiB"</td>
                                    <td class="actions">
                                        <button class="btn ghost sm">"Logs"</button>
                                        <button class="btn sm">"Stop"</button>
                                    </td>
                                </tr>
                                <tr>
                                    <td>"gemma-4-12b"</td>
                                    <td>
                                        <span class="chip off">"stopped"</span>
                                    </td>
                                    <td class="num">"9.8 GiB"</td>
                                    <td class="actions">
                                        <button class="btn ghost sm">"Logs"</button>
                                        <button class="btn sm row-main">"Start"</button>
                                        <button class="btn danger sm">"Delete"</button>
                                    </td>
                                </tr>
                            </tbody>
                        </table>
                    </PrefGroup>
                </div>
            </div>
            <SaveToast
                dirty_count=dirty_count
                invalid=invalid
                saving=Signal::stored(false)
                error=Signal::stored(None::<String>)
                on_save=discard
                on_discard=discard
            />
        </PageFrame>
    }
}
