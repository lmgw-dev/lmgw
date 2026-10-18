//! A key's device-tools grant (client-apps design L16 as changed
//! 2026-10-15): the paired devices' hosted labels whose tools the key
//! reaches, whatever its tool scope's mode — Kai desktop's `kai`, say. The
//! tool scope then picks which of those tools it sees, as it does any
//! server's; without a grant no key reaches another device's tools.
//!
//! The labels a device hosts now are checkboxes. Any other label a device
//! could host goes in the free entry: a phone may be paired before its
//! desktop ever connected, and the gateway keeps such a label until a
//! device hosts it, which the editor and the device row say. Used by the
//! pairing form and the key policy dialog.

use leptos::prelude::*;
use lmgw_api_types::KeyRow;

/// One label a paired device hosts now, as a checkbox reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct HostedLabel {
    pub label: String,
    /// The hosting device's name, without `device:`.
    pub device: String,
    /// The hosting device's key: it reaches its own label anyway.
    pub key_id: i64,
}

/// The labels the paired devices host now, from the keys list, by label.
pub(super) fn hosted_labels(keys: &[KeyRow]) -> Vec<HostedLabel> {
    let mut out: Vec<HostedLabel> = keys
        .iter()
        .filter(|k| k.kind == "device")
        .filter_map(|k| {
            let label = k.hosts_label.as_deref()?.trim();
            (!label.is_empty()).then(|| HostedLabel {
                label: label.to_string(),
                device: k
                    .name
                    .strip_prefix("device:")
                    .unwrap_or(&k.name)
                    .to_string(),
                key_id: k.id,
            })
        })
        .collect();
    out.sort_by_key(|h| h.label.to_ascii_lowercase());
    out
}

fn hosts(hosted: &[HostedLabel], label: &str) -> bool {
    hosted.iter().any(|h| h.label.eq_ignore_ascii_case(label))
}

/// A grant split for the editor: the labels a device hosts now (ticked, in
/// the hosted list's spelling) and the rest as the free entry's text.
pub(super) fn split_grant(grant: &[String], hosted: &[HostedLabel]) -> (Vec<String>, String) {
    let mut checked = Vec::new();
    let mut other = Vec::new();
    for label in grant.iter().map(|l| l.trim()).filter(|l| !l.is_empty()) {
        match hosted.iter().find(|h| h.label.eq_ignore_ascii_case(label)) {
            Some(h) => checked.push(h.label.clone()),
            None => other.push(label),
        }
    }
    (checked, other.join(", "))
}

/// The grant the editor holds: the ticked labels, then the free entry's
/// (split on commas and white space), each once in any case. The gateway
/// checks each label's syntax and says which it refuses.
pub(super) fn merge_grant(checked: &[String], other: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let typed = other
        .split(|c: char| c == ',' || c.is_whitespace())
        .map(str::trim);
    for label in checked.iter().map(|l| l.trim()).chain(typed) {
        if !label.is_empty() && !out.iter().any(|l| l.eq_ignore_ascii_case(label)) {
            out.push(label.to_string());
        }
    }
    out
}

/// The granted labels no paired device hosts now.
pub(super) fn unhosted(grant: &[String], hosted: &[HostedLabel]) -> Vec<String> {
    grant
        .iter()
        .filter(|l| !hosts(hosted, l))
        .cloned()
        .collect()
}

/// How a key reaches a tool of the source labelled `source`, the gateway's
/// rule (L16 as changed 2026-10-15): `Some(true)` whole — the device's own
/// hosted label —, `Some(false)` not at all — another device's label the
/// grant does not name —, `None` for the tool scope to decide (any other
/// source, or a granted label).
pub(super) fn hosted_reach(
    source: &str,
    hosted: &[HostedLabel],
    own: Option<i64>,
    grant: &[String],
) -> Option<bool> {
    let h = hosted
        .iter()
        .find(|h| h.label.eq_ignore_ascii_case(source))?;
    if Some(h.key_id) == own {
        return Some(true);
    }
    (!grant.iter().any(|l| l.eq_ignore_ascii_case(source))).then_some(false)
}

/// The hosted labels an `allow` tool scope names but `grant` does not hold
/// (a device's own label aside): such a pattern filters tools it cannot
/// reach until the grant is given. Empty for any other mode.
pub(super) fn needs_grant(
    mode: &str,
    patterns: &str,
    grant: &[String],
    own: Option<i64>,
    hosted: &[HostedLabel],
) -> Vec<String> {
    if mode != "allow" {
        return Vec::new();
    }
    let labels: Vec<String> = hosted
        .iter()
        .filter(|h| Some(h.key_id) != own)
        .map(|h| h.label.clone())
        .collect();
    lmgw_api_types::scope::ungranted_named(patterns, grant, &labels)
        .into_iter()
        .map(str::to_string)
        .collect()
}

/// The words of the warning on a pattern that names device tools.
pub(super) const NEEDS_GRANT: &str = "names device tools: they need the Device tools grant";

/// One amber chip per label in `labels`, for a row.
pub(super) fn needs_grant_chips(labels: Vec<String>) -> impl IntoView {
    labels
        .into_iter()
        .map(|l| {
            view! {
                " "
                <span class="chip warn key-kind mono-sm" title=NEEDS_GRANT>
                    {format!("{l}__: needs Device tools")}
                </span>
            }
        })
        .collect_view()
}

/// A grant in words, as the review step and a row say it: "kai, desk" or
/// "none".
pub(super) fn grant_words(grant: &[String]) -> String {
    if grant.is_empty() {
        "none".to_string()
    } else {
        grant.join(", ")
    }
}

/// What the editor holds, in signals the form reads when it saves.
#[derive(Clone, Copy)]
pub(super) struct DeviceToolsState {
    checked: RwSignal<Vec<String>>,
    other: RwSignal<String>,
}

impl DeviceToolsState {
    pub fn new() -> Self {
        Self {
            checked: RwSignal::new(Vec::new()),
            other: RwSignal::new(String::new()),
        }
    }

    /// Show `grant`, split over the labels hosted now.
    pub fn load(&self, grant: &[String], hosted: &[HostedLabel]) {
        let (checked, other) = split_grant(grant, hosted);
        self.checked.set(checked);
        self.other.set(other);
    }

    /// The grant as the form sends it (tracked).
    pub fn labels(&self) -> Vec<String> {
        merge_grant(&self.checked.get(), &self.other.get())
    }
}

/// The "device tools" field: a checkbox per label a paired device hosts now
/// (but `own`'s, which a device reaches anyway), a free entry for any
/// other, and what a granted label no device hosts means.
#[component]
pub(super) fn DeviceToolsEditor(
    state: DeviceToolsState,
    hosted: Signal<Vec<HostedLabel>>,
    /// The key being edited: a device's own hosted label is not offered.
    #[prop(into)]
    own: Signal<Option<i64>>,
    #[prop(into)] disabled: Signal<bool>,
    /// The key's tool scope as the form holds it: mode and patterns.
    #[prop(into)]
    scope: Signal<(String, String)>,
) -> impl IntoView {
    let unmet = Memo::new(move |_| {
        let (mode, patterns) = scope.get();
        needs_grant(&mode, &patterns, &state.labels(), own.get(), &hosted.get())
    });
    let offered = Memo::new(move |_| {
        let own = own.get();
        hosted
            .get()
            .into_iter()
            .filter(|h| Some(h.key_id) != own)
            .collect::<Vec<_>>()
    });
    let waiting = Memo::new(move |_| unhosted(&state.labels(), &hosted.get()));
    view! {
        <div class="row device-tools-row">
            <span class="lbl">"device tools"</span>
            <div class="device-tools">
                <Show
                    when=move || !offered.get().is_empty()
                    fallback=|| view! {
                        <span class="dim mini-note">"no other paired device hosts tools now"</span>
                    }
                >
                    <For each=move || offered.get() key=|h| h.label.clone() let:h>
                        {
                            let label = h.label.clone();
                            let toggle = label.clone();
                            view! {
                                <label class="check">
                                    <input
                                        type="checkbox"
                                        disabled=move || disabled.get()
                                        prop:checked=move || {
                                            state.checked.with(|c| c.iter().any(|l| l.eq_ignore_ascii_case(&label)))
                                        }
                                        on:change=move |ev| {
                                            let on = event_target_checked(&ev);
                                            state.checked.update(|c| {
                                                c.retain(|l| !l.eq_ignore_ascii_case(&toggle));
                                                if on {
                                                    c.push(toggle.clone());
                                                }
                                            });
                                        }
                                    />
                                    <span class="mono-sm">{format!("{}__", h.label)}</span>
                                    <span class="dim">{format!("device '{}'", h.device)}</span>
                                </label>
                            }
                        }
                    </For>
                </Show>
                <input
                    class="input mono"
                    placeholder="other labels, e.g. kai"
                    aria-label="Other hosted labels, comma-separated"
                    disabled=move || disabled.get()
                    prop:value=move || state.other.get()
                    on:input=move |ev| state.other.set(event_target_value(&ev))
                />
            </div>
        </div>
        <div class="mini-note dim">
            "The paired devices' own tools this key may use — Kai desktop's, say — whatever its tool scope's mode; the tool scope then picks which of them it sees. Without one here, no key reaches another device's tools."
        </div>
        <Show when=move || !unmet.get().is_empty()>
            <div class="mini-note notice warn">
                {move || {
                    format!(
                        "A tool scope pattern {NEEDS_GRANT} ({}). Tick it above and the pattern \
                         will filter its tools.",
                        unmet.get().iter().map(|l| format!("{l}__…")).collect::<Vec<_>>().join(", ")
                    )
                }}
            </div>
        </Show>
        <Show when=move || !waiting.get().is_empty()>
            <div class="mini-note notice warn">
                {move || {
                    format!(
                        "No paired device hosts {} now: the grant waits until one does.",
                        waiting.get().join(", ")
                    )
                }}
            </div>
        </Show>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hosted() -> Vec<HostedLabel> {
        vec![
            HostedLabel {
                label: "kai".into(),
                device: "kai desktop".into(),
                key_id: 3,
            },
            HostedLabel {
                label: "desk".into(),
                device: "desk".into(),
                key_id: 4,
            },
        ]
    }

    fn v(l: &[&str]) -> Vec<String> {
        l.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn the_hosted_labels_are_the_device_rows_with_a_label_by_label() {
        let rows = vec![
            KeyRow {
                id: 3,
                name: "device:kai desktop".into(),
                kind: "device".into(),
                hosts_label: Some("kai".into()),
                ..Default::default()
            },
            KeyRow {
                id: 4,
                name: "device:phone".into(),
                kind: "device".into(),
                ..Default::default()
            },
            KeyRow {
                id: 5,
                name: "device:desk".into(),
                kind: "device".into(),
                hosts_label: Some("Desk".into()),
                ..Default::default()
            },
            KeyRow {
                id: 6,
                name: "ci".into(),
                kind: "key".into(),
                ..Default::default()
            },
        ];
        let got: Vec<(String, String, i64)> = hosted_labels(&rows)
            .into_iter()
            .map(|h| (h.label, h.device, h.key_id))
            .collect();
        assert_eq!(
            got,
            vec![
                ("Desk".to_string(), "desk".to_string(), 5),
                ("kai".to_string(), "kai desktop".to_string(), 3),
            ]
        );
    }

    #[test]
    fn a_grant_splits_into_ticks_and_free_entry_and_merges_back() {
        let (checked, other) = split_grant(&v(&["KAI", "phone-tools", "later"]), &hosted());
        assert_eq!(checked, v(&["kai"]));
        assert_eq!(other, "phone-tools, later");
        assert_eq!(
            merge_grant(&checked, &other),
            v(&["kai", "phone-tools", "later"])
        );
        assert_eq!(
            merge_grant(&v(&["kai"]), " Kai,  x\ny ,, "),
            v(&["kai", "x", "y"]),
            "each once, any separator"
        );
        assert_eq!(merge_grant(&[], ""), Vec::<String>::new());
    }

    #[test]
    fn a_hosted_source_is_reached_by_its_host_and_a_grant_only() {
        let g = v(&["Kai"]);
        assert_eq!(hosted_reach("kai", &hosted(), None, &g), None, "granted");
        assert_eq!(hosted_reach("desk", &hosted(), None, &g), Some(false));
        assert_eq!(hosted_reach("desk", &hosted(), Some(4), &g), Some(true));
        assert_eq!(hosted_reach("github", &hosted(), None, &[]), None);
    }

    #[test]
    fn a_pattern_naming_an_ungranted_label_is_flagged() {
        let allow = "kai__*\ndesk__x\ngithub__*";
        assert_eq!(
            needs_grant("allow", allow, &[], None, &hosted()),
            v(&["kai", "desk"])
        );
        assert_eq!(
            needs_grant("allow", allow, &v(&["KAI"]), None, &hosted()),
            v(&["desk"])
        );
        assert_eq!(
            needs_grant("allow", allow, &[], Some(4), &hosted()),
            v(&["kai"]),
            "a device's own label needs no grant"
        );
        assert!(needs_grant("deny", allow, &[], None, &hosted()).is_empty());
    }

    #[test]
    fn a_label_no_device_hosts_is_said() {
        assert_eq!(unhosted(&v(&["Kai", "later"]), &hosted()), v(&["later"]));
        assert_eq!(grant_words(&[]), "none");
        assert_eq!(grant_words(&v(&["kai", "desk"])), "kai, desk");
    }
}
