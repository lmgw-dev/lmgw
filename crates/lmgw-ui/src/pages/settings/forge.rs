//! Settings — the forge-token list (container-builds §7): its rows, patch and field.

use super::*;

// ---------------------------------------------------------------------------
// Forge tokens (container-builds §7)
// ---------------------------------------------------------------------------

/// The host a forge token is always offered for: the official repositories
/// live there, and its anonymous API limit is the one that bites.
const GITHUB: &str = "github.com";

/// One host of the forge-token list, as the draft holds it.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub(super) struct ForgeRow {
    /// The row's identity in the list: the host of a stored (or the GitHub)
    /// row, `new-<n>` for one added here.
    pub(super) key: String,
    pub(super) host: String,
    /// A token to set on Save; empty keeps what is stored.
    pub(super) token: String,
    /// The server holds a token for this host.
    pub(super) stored: bool,
    /// Erase the stored token on Save.
    pub(super) clear: bool,
}

impl ForgeRow {
    /// Typed in here, not read from the server: its host is editable and it
    /// can be removed.
    pub(super) fn added(&self) -> bool {
        self.key.starts_with("new-")
    }
}

/// The rows for what the server holds (`{"github.com": "<set>", …}`):
/// GitHub first whether or not it has a token, then every other host.
pub(super) fn forge_baseline(stored: &Value) -> Vec<ForgeRow> {
    let mut hosts: Vec<String> = stored
        .as_object()
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default();
    hosts.sort();
    let row = |host: &str, stored: bool| ForgeRow {
        key: host.to_string(),
        host: host.to_string(),
        stored,
        ..ForgeRow::default()
    };
    let mut rows = vec![row(GITHUB, hosts.iter().any(|h| h == GITHUB))];
    rows.extend(hosts.iter().filter(|h| *h != GITHUB).map(|h| row(h, true)));
    rows
}

pub(super) fn forge_rows(v: &Value) -> Vec<ForgeRow> {
    serde_json::from_value(v.clone()).unwrap_or_default()
}

/// The save's two halves: the tokens to set by host, and the hosts to clear.
pub(super) fn forge_patch(rows: &[ForgeRow]) -> (BTreeMap<String, String>, Vec<String>) {
    let mut set = BTreeMap::new();
    let mut clear = Vec::new();
    for r in rows {
        let host = r.host.trim().to_ascii_lowercase();
        if r.clear {
            if r.stored {
                clear.push(host);
            }
        } else if !r.token.trim().is_empty() && !host.is_empty() {
            set.insert(host, r.token.trim().to_string());
        }
    }
    (set, clear)
}

/// What the server holds after `rows` were saved, when it could not be read
/// back: a set token is stored, a cleared one gone, every box empty.
pub(super) fn forge_after(rows: &[ForgeRow]) -> Vec<ForgeRow> {
    rows.iter()
        .filter_map(|r| {
            let host = r.host.trim().to_ascii_lowercase();
            let stored = if r.clear {
                false
            } else {
                r.stored || !r.token.trim().is_empty()
            };
            (stored || host == GITHUB).then(|| ForgeRow {
                key: host.clone(),
                host,
                stored,
                ..ForgeRow::default()
            })
        })
        .collect()
}

/// A forge host as `settings_set_full` accepts it: the host alone, lowercase
/// labels, an optional port (`git.example:8443`). The server checks again.
pub(super) fn forge_host_ok(h: &str) -> bool {
    let h = h.trim().to_ascii_lowercase();
    let (name, port) = match h.split_once(':') {
        Some((n, p)) => (n, Some(p)),
        None => (h.as_str(), None),
    };
    let name_ok = !name.is_empty()
        && name.split('.').all(|l| {
            !l.is_empty()
                && !l.starts_with('-')
                && !l.ends_with('-')
                && l.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        });
    name_ok && port.is_none_or(|p| p.parse::<u16>().is_ok_and(|n| n > 0))
}

/// What blocks Save in the forge-token list, in words.
pub(super) fn forge_error(rows: &[ForgeRow]) -> Option<String> {
    let mut seen: Vec<String> = Vec::new();
    for r in rows {
        let host = r.host.trim().to_ascii_lowercase();
        let token = r.token.trim();
        if r.added() {
            if host.is_empty() {
                return Some(if token.is_empty() {
                    "an added host is empty: fill it in, or remove the row".to_string()
                } else {
                    "a token needs its host".to_string()
                });
            }
            if !forge_host_ok(&host) {
                return Some(format!(
                    "\u{201c}{host}\u{201d} is not a host name: the host alone, e.g. \
                     git.example.com or git.example:8443"
                ));
            }
            if token.is_empty() {
                return Some(format!("the token for {host} is empty"));
            }
        }
        if token.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Some(format!("the token for {host} contains whitespace"));
        }
        if seen.contains(&host) {
            return Some(format!("{host} is listed twice"));
        }
        seen.push(host);
    }
    None
}

// ---------------------------------------------------------------------------
// The forge-token list
// ---------------------------------------------------------------------------

/// One row per host (GitHub always, then every stored host, then any added
/// here): a masked box that sets a new token, a clear tick on a stored one,
/// Remove on an added one. A stored token is never shown, only that it is
/// there. The whole list is one `Raw` draft value ([`ForgeRow`]s), turned
/// into `forge_tokens` / `clear_forge_tokens` by [`finish`].
#[component]
pub(super) fn ForgeTokensField(
    page: Page,
    id: String,
    hidden: Signal<bool>,
    error: Signal<Option<String>>,
    warn: Signal<Option<String>>,
) -> impl IntoView {
    const K: &str = "forge_tokens";
    let form = page.form;
    let rows = Memo::new(move |_| forge_rows(&form.value(K)));
    let edit = move |f: &dyn Fn(&mut Vec<ForgeRow>)| {
        let mut r = form
            .draft
            .with_untracked(|d| d.get(K).map(forge_rows))
            .unwrap_or_default();
        f(&mut r);
        form.set_value(K, json!(r));
    };
    let next = StoredValue::new(0u32);
    let add = move |_| {
        // Past any `new-<n>` a Discard left behind in the numbering.
        let n = next.get_value() + 1;
        next.set_value(n);
        edit(&|r| {
            r.push(ForgeRow {
                key: format!("new-{n}"),
                ..ForgeRow::default()
            })
        });
    };
    let row_view = move |key: String| {
        // `Copy` handles on the row, for the closures below.
        let key_s = StoredValue::new(key.clone());
        let get =
            move || key_s.with_value(|k| rows.with(|rs| rs.iter().find(|r| &r.key == k).cloned()));
        let set = move |f: &dyn Fn(&mut ForgeRow)| {
            key_s.with_value(|k| {
                edit(&|rs| {
                    if let Some(r) = rs.iter_mut().find(|r| &r.key == k) {
                        f(r);
                    }
                })
            })
        };
        let added = key.starts_with("new-");
        let host_cell = if added {
            view! {
                <input
                    class="input mono forge-host"
                    placeholder="git.example.com"
                    aria-label="Forge host"
                    spellcheck="false"
                    autocomplete="off"
                    prop:value=move || get().map(|r| r.host).unwrap_or_default()
                    on:input=move |ev| {
                        let v = event_target_value(&ev);
                        set(&|r| r.host = v.clone());
                    }
                />
            }
            .into_any()
        } else {
            view! { <span class="mono forge-host forge-host-fixed">{key.clone()}</span> }.into_any()
        };
        let stored = move || get().is_some_and(|r| r.stored);
        let clearing = move || get().is_some_and(|r| r.clear);
        let token = move || get().map(|r| r.token).unwrap_or_default();
        let remove = move |_| key_s.with_value(|k| edit(&|rs| rs.retain(|r| &r.key != k)));
        let label = format!("Token for {key}");
        view! {
            <div class="forge-row">
                {host_cell}
                <input
                    class="input mono forge-token"
                    type="password"
                    autocomplete="off"
                    aria-label=label
                    placeholder=move || {
                        if stored() { "type to replace" } else { "token" }
                    }
                    disabled=clearing
                    prop:value=token
                    on:input=move |ev| {
                        let v = event_target_value(&ev);
                        set(&|r| r.token = v.clone());
                    }
                />
                {move || {
                    if added {
                        view! {
                            <button type="button" class="btn ghost sm" on:click=remove>
                                "Remove"
                            </button>
                        }
                            .into_any()
                    } else if stored() {
                        view! {
                            <span class="chip ok forge-state">"stored"</span>
                            <label class="check" title="Erased on Save">
                                <input
                                    type="checkbox"
                                    prop:checked=clearing
                                    on:change=move |ev| {
                                        let on = event_target_checked(&ev);
                                        set(&|r| {
                                            r.clear = on;
                                            if on {
                                                r.token.clear();
                                            }
                                        });
                                    }
                                />
                                "clear"
                            </label>
                        }
                            .into_any()
                    } else {
                        view! { <span class="chip off forge-state">"not set"</span> }.into_any()
                    }
                }}
            </div>
        }
    };
    view! {
        <Field
            label="Forge tokens"
            unit="write-only"
            hint="Per host; an empty box keeps what is stored. Saved with the rest of the page."
            dirty=form.dirty_signal(K)
            error=error
            warn=warn
            id=id
            hidden=hidden
        >
            <div class="forge-rows">
                <For each=move || rows.get().into_iter().map(|r| r.key) key=|k| k.clone() let:key>
                    {row_view(key)}
                </For>
            </div>
            <button type="button" class="btn sm forge-add" on:click=add>
                "Add host"
            </button>
        </Field>
    }
}
