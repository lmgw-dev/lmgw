use leptos::prelude::*;
use leptos::task::spawn_local;
use lmgw_api_types::{AgentDetail, SettingsFull};
use serde_json::{json, Value};

use super::*;
use crate::widgets::{use_toasts, Explain};

// ---------------------------------------------------------------------------
// App (container-runtime §3.3, §7)
// ---------------------------------------------------------------------------

/// The App tab: the agent's own UI in an iframe at the agent's **own origin**,
/// `http://<id>.<suffix>:<port>/` (origins §4.1), under one line of state and
/// controls — the frame takes the rest of the window.
///
/// The frame is therefore a different site from the dashboard, and §4.9's
/// three standing lines say what that costs: a name that may not resolve, a
/// `*.localhost` that only resolves here, and an app that may refuse to be
/// embedded at all. None of the three is detectable from this side — a
/// cross-origin frame's load failure is opaque by design — so they are
/// standing lines and not diagnoses. The first two show above the frame when
/// they apply; the third lives in Info.
///
/// **The iframe is the Start button.** Loading it makes a proxied request, and
/// a proxied request starts the container — which is the whole of "on demand".
/// The explicit Start is for the case where the owner wants the container up
/// (and its tools on `/mcp`) without looking at its page.
///
/// Every number here is the manifest's own, printed rather than assumed: the
/// idle window, the start timeout and the port. `0` means what it says
/// everywhere else in the runtime — never idle-stop, and wait as long as the
/// start takes.
#[component]
pub(super) fn AppTab(
    d: AgentDetail,
    /// What the service container is started with (mounts §5.7, §5.8), read
    /// from the detail document by the page.
    mounts: RwSignal<Vec<ServiceMount>>,
    reload: Callback<()>,
) -> impl IntoView {
    let Some(svc) = d.service.clone() else {
        return view! {
            <div class="fill-pane">
                <div class="empty">"This agent declares no service."</div>
            </div>
        }
        .into_any();
    };
    let id = StoredValue::new(d.id.clone());
    let toasts = use_toasts();
    let busy = RwSignal::new(false);
    // Bumped on every Start/Stop so the iframe is re-created rather than left
    // pointing at a container that has since been stopped.
    let generation = RwSignal::new(0u32);
    // The name a resolver is asked for, which is the origin's host and not the
    // origin: the `/etc/hosts` line below has to be pasteable as it stands.
    let host = origin_host(&svc.origin);
    let resolves = svc.origin_resolves;
    // §4.9's second line needs two gateway settings this agent's detail does
    // not carry — `bind_addr` and `agent_origin_suffix`. There is no shared
    // settings context in this dashboard; every page that needs one reads
    // `/api/settings-full` itself (settings.rs, docs_search.rs), so this tab
    // does too, and the line stays away until that read lands rather than
    // assuming the default suffix.
    let settings = LocalResource::new(|| crate::api::get::<SettingsFull>("/api/settings-full"));
    let lan_host = move || {
        settings
            .get()
            .and_then(|r| r.ok())
            .and_then(|s| lan_origin_host(&s.bind_addr, &s.agent_origin_suffix))
    };
    let src = {
        // The agent's own origin (origins §4.1), not a path under the
        // dashboard's: the frame is a different site, which is what keeps the
        // app's JS out of this page.
        let origin = svc.origin.clone();
        move || format!("{origin}?v={}", generation.get())
    };

    let press = move |start: bool| {
        if busy.get_untracked() {
            return;
        }
        busy.set(true);
        spawn_local(async move {
            let op = if start {
                "agent_service_start"
            } else {
                "agent_service_stop"
            };
            let res = crate::api::post::<Value, _>(
                &format!("/api/op/{op}"),
                &json!({ "id": id.get_value() }),
            )
            .await;
            busy.set(false);
            match res {
                Ok(v) => {
                    toasts.ok(v
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("done")
                        .to_string());
                    generation.update(|g| *g += 1);
                    reload.try_run(());
                }
                Err(e) => toasts.err(e.to_string()),
            }
        });
    };

    // §3.4's dev override. While it is set the proxy goes to that URL, nothing
    // is started, and the badge says which — a page that looked exactly like
    // the container's would be the worst possible answer here.
    let dev_url = StoredValue::new(d.dev_url.clone());
    let dev_active = !d.dev_url.is_empty();
    let dev_draft = RwSignal::new(d.dev_url.clone());
    let set_dev = move |clear: bool| {
        if busy.get_untracked() {
            return;
        }
        let url = dev_draft.get_untracked().trim().to_string();
        if !clear && url.is_empty() {
            toasts.err("type the dev server's URL first, e.g. http://127.0.0.1:5173");
            return;
        }
        busy.set(true);
        spawn_local(async move {
            let body = if clear {
                json!({ "id": id.get_value(), "url": Value::Null })
            } else {
                json!({ "id": id.get_value(), "url": url })
            };
            let res = crate::api::post::<Value, _>("/api/op/agent_dev_url_set", &body).await;
            busy.set(false);
            match res {
                Ok(v) => {
                    toasts.ok(v
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("done")
                        .to_string());
                    generation.update(|g| *g += 1);
                    reload.try_run(());
                }
                Err(e) => toasts.err(e.to_string()),
            }
        });
    };

    let running = svc.running;
    let starting = svc.starting;
    let enabled = d.enabled;
    let status = if dev_active {
        format!("dev: {}", dev_url.get_value())
    } else if svc.starting {
        "starting…".to_string()
    } else if running {
        format!(
            "running as {} on 127.0.0.1:{}",
            svc.container, svc.host_port
        )
    } else if !d.enabled {
        "not running — a disabled agent's app does not start".to_string()
    } else {
        "not running — the first request to the app starts it".to_string()
    };
    let idle_line = match svc.idle_seconds {
        0 => "stays up until you stop it (service.idle_seconds = 0)".to_string(),
        n => format!("stops after {n}s with no request (service.idle_seconds)"),
    };
    let start_line = match svc.start_timeout_seconds {
        0 => "a start waits as long as the container takes (start_timeout_seconds = 0)".to_string(),
        n => format!("a start waits up to {n}s for its health probe"),
    };
    let last = svc
        .idle_seconds_now
        .map(|n| match svc.in_flight {
            0 => format!("last request {n}s ago"),
            1 => "1 request in flight".to_string(),
            k => format!("{k} requests in flight"),
        })
        .unwrap_or_default();
    let health = if svc.health_path.is_empty() {
        format!("a TCP connect to port {}", svc.port)
    } else {
        format!("GET {} on port {}", svc.health_path, svc.port)
    };
    let provides = svc.provides_mcp.clone().map(|path| {
        if dev_active {
            // Nothing to start means nothing to wait for: the aggregate lists a
            // dev row's tools straight away (container-runtime §3.4).
            format!(
                "its tools are registered as the MCP server 'agent:{}' (container path {path}), \
                 and while the dev server is in force they are listed whenever it answers — \
                 there is no container to start, so nothing has to be woken first.",
                d.id
            )
        } else {
            format!(
                "its tools are registered as the MCP server 'agent:{}' (container path {path}). \
                 An aggregate tools/list never starts a container, so while this app is stopped \
                 its tools are not listed and the MCP page shows the row as sleeping; a \
                 tools/call on one of them, a chat thread attaching the label '{}', or Start \
                 brings it up.",
                d.id, d.id
            )
        }
    });
    // §7: the container's own log. The detail payload carries an excerpt so the
    // page draws itself without a second round trip; the field below is how the
    // owner asks for more of it, because for a detached service container this
    // tail is the *only* account there is (no run ledger, no job row) and
    // twelve lines was a cap with no dial (final review).
    let log = RwSignal::new(svc.log_tail.clone());
    let log_lines = RwSignal::new(svc.log_tail_lines.to_string());
    let log_shown = RwSignal::new(svc.log_tail_lines);
    let log_busy = RwSignal::new(false);
    let base = svc.origin.clone();
    let log_id = d.id.clone();
    let fetch_log = move |_| {
        let want: usize = log_lines.get_untracked().trim().parse().unwrap_or(0);
        let id = log_id.clone();
        log_busy.set(true);
        spawn_local(async move {
            let res = crate::api::post::<Value, _>(
                "/api/op/agent_service_log",
                &json!({ "id": id, "lines": want }),
            )
            .await;
            log_busy.set(false);
            match res {
                Ok(v) => {
                    log.set(
                        v.get("log")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                    );
                    log_shown.set(want);
                }
                Err(e) => log.set(e.to_string()),
            }
        });
    };
    let log_count = move || log.with(|t| t.lines().filter(|l| !l.trim().is_empty()).count());

    // One drawer at a time over the frame: the frame is the point of the tab,
    // the drawers are what you look up about it.
    let drawer = RwSignal::new("");
    let toggle = move |which: &'static str| {
        drawer.update(|d| *d = if *d == which { "" } else { which });
    };
    let seg = move |which: &'static str| {
        move || {
            if drawer.get() == which {
                "seg-btn active"
            } else {
                "seg-btn"
            }
        }
    };
    let status_title = status.clone();

    view! {
        <div class="app-bar">
            <span class=if dev_active || running { "chip app-status" } else { "chip off app-status" } title=status_title>
                {status}
            </span>
            // The server asked its own resolver for this name while it
            // rendered the tab (§4.9). False is a verdict, not an error:
            // Chrome and Firefox resolve `*.localhost` whatever the system
            // says, so the frame below may well load anyway.
            <span
                class=if resolves { "chip ok" } else { "chip warn" }
                title=if resolves {
                    "the gateway's own resolver answers for this name"
                } else {
                    "the gateway's own resolver has no answer for this name"
                }
            >
                {if resolves { "origin resolves" } else { "origin does not resolve" }}
            </span>
            {(!last.is_empty() && !dev_active).then(|| view! { <span class="dim mono-sm">{last}</span> })}
            <span class="spacer"></span>
            <div class="seg">
                <button
                    class=seg("info")
                    title="how the app is served and kept, and the folders it holds"
                    on:click=move |_| toggle("info")
                >
                    "Info"
                </button>
                <button
                    class=seg("dev")
                    title="serve the app from a dev server on this machine instead of its image"
                    on:click=move |_| toggle("dev")
                >
                    "Dev server"
                    {dev_active.then(|| view! { <span class="count attn">"on"</span> })}
                </button>
                <button class=seg("log") title="what the container itself wrote" on:click=move |_| toggle("log")>
                    "Log"
                    <span class="count">{log_count}</span>
                </button>
            </div>
            // `_blank` on purpose: in the desktop shell this leaves the
            // window and opens in the system browser, which is the one view
            // of the app that no framing rule can spoil (§4.9).
            <a
                class="btn ghost sm"
                href=base.clone()
                target="_blank"
                title="open the app on its own origin, outside this window"
            >
                "Open full page"
            </a>
            <button
                class="btn sm"
                // `svc.starting` too: a start is already in flight, one start
                // per agent is the contract, and a button that looks pressable
                // during the slowest part of the operation reads as "nothing
                // happened".
                disabled=move || busy.get() || dev_active || starting || !enabled
                on:click=move |_| press(true)
                title=if !enabled {
                    "this agent is disabled; enable it on the catalog before its app can start"
                } else if dev_active {
                    "this agent is served from its dev_url; no container is started for it"
                } else if starting {
                    "a start is already in flight"
                } else {
                    "start the container without opening its page"
                }
            >
                "Start"
            </button>
            <button class="btn ghost sm" disabled=move || busy.get() on:click=move |_| press(false)>
                "Stop"
            </button>
        </div>

        // §4.9's first two standing lines, each shown only when it applies.
        {(!resolves)
            .then(|| {
                view! {
                    <div class="notice warn">
                        <code>{host.clone()}</code>
                        " does not resolve on this machine. Add "
                        <code>{format!("127.0.0.1 {host}")}</code> " to "
                        <code>"/etc/hosts"</code>
                        ", or set an agent origin suffix under "
                        <a href=crate::pages::settings_href("agent_origin_suffix")>
                            "Settings → Agents & tools"
                        </a>
                        " that your DNS answers for."
                    </div>
                }
            })}
        {move || {
            lan_host()
                .map(|h| {
                    view! {
                        <div class="notice warn">
                            "The gateway is bound to " <code>{h}</code> "; "
                            <code>"*.localhost"</code>
                            " only resolves on this machine. Set an agent origin suffix under "
                            <a href=crate::pages::settings_href("agent_origin_suffix")>
                                "Settings → Agents & tools"
                            </a>
                            " with a wildcard record to reach agent UIs from elsewhere."
                        </div>
                    }
                })
        }}

        <Show when=move || drawer.get() == "info">
            <div class="app-drawer">
                <p class="dim mini-note">
                    "Served from the agent's own container at "
                    <a href=base.clone() target="_blank">
                        <code>{base.clone()}</code>
                    </a>
                    ", an origin of its own. lmgw proxies it and rewrites nothing: the container "
                    "is told its origin (LMGW_APP_ORIGIN) and writes its own URLs."
                </p>
                <p class="dim mini-note">
                    {idle_line.clone()} ". " {start_line.clone()} ". Health: " {health.clone()} "."
                </p>
                {provides.clone().map(|p| view! { <p class="dim mini-note">{p}</p> })}
                // The third standing line: from this side a frame that was
                // refused and a frame that loaded look exactly the same.
                <p class="dim mini-note">
                    "The frame is a different site from the dashboard. An app that forbids "
                    "embedding renders blank, and an app that keeps its own session cookie may "
                    "not keep it in here — WebKitGTK blocks third-party cookies by default. "
                    <i>"Open full page"</i> " is the reliable view."
                </p>
                // mounts §5.8: what this container can see of the filesystem,
                // host path to container path — and re-pointing one of these
                // stops the container.
                <Show when=move || !mounts.get().is_empty()>
                    <div class="mini-head">"Mounts"</div>
                    <table class="data">
                        <thead>
                            <tr>
                                <th>"Field"</th>
                                <th>"Host → container"</th>
                                <th>"Access"</th>
                            </tr>
                        </thead>
                        <tbody>
                            <For each=move || mounts.get() key=|m: &ServiceMount| m.field.clone() let:m>
                                <tr>
                                    <td class="mono-sm">{m.field.clone()}</td>
                                    <td
                                        class="mono-sm clip"
                                        title=format!("{} → {}", m.host.clone().unwrap_or_default(), m.inside)
                                    >
                                        {format!("{} → {}", m.host.clone().unwrap_or_default(), m.inside)}
                                    </td>
                                    <td class="mono-sm">{format!("{} · {}", m.access, m.kind)}</td>
                                </tr>
                            </For>
                        </tbody>
                    </table>
                    <p class="dim mini-note">
                        "Changing one of these under Config stops the app container: it is holding a "
                        "mount you have re-pointed, and the next request starts it against the new one."
                    </p>
                </Show>
            </div>
        </Show>

        // §3.4: the hot-reload loop. A row setting, never the manifest's — a
        // manifest naming localhost:5173 would ship a broken agent.
        <Show when=move || drawer.get() == "dev">
            <div class="app-drawer">
                <div class="row dev-row">
                    <label class="lab-label" for="agent-dev-url">"Dev server (dev_url)"</label>
                    <input
                        id="agent-dev-url"
                        class="input mono-sm"
                        placeholder="http://127.0.0.1:5173"
                        prop:value=move || dev_draft.get()
                        on:input=move |ev| dev_draft.set(event_target_value(&ev))
                    />
                    <button class="btn sm" disabled=move || busy.get() on:click=move |_| set_dev(false)>
                        "Use dev server"
                    </button>
                    <button
                        class="btn ghost sm"
                        disabled=move || busy.get() || !dev_active
                        on:click=move |_| set_dev(true)
                    >
                        "Clear"
                    </button>
                </div>
                <Explain
                    summary="While set, the app is served from that URL and no container is started for it."
                    persist="agents.explain.devurl"
                >
                    "A running app container is stopped. Runs and applies still use the image, "
                    "because their dev loop is a rebuild. http(s) on loopback only (not a LAN "
                    "address: this proxy has no authentication), never lmgw's own port, and "
                    "never exported."
                </Explain>
            </div>
        </Show>

        // §7: what the container itself said. Not the ledger — a service
        // reports nothing through it — so `podman logs` is the whole account
        // there is, and the count says how much of it this is.
        <Show when=move || drawer.get() == "log">
            <div class="app-drawer">
                <div class="row">
                    <div class="mini-head">
                        {move || match log_shown.get() {
                            0 => "Container log — all of it".to_string(),
                            n => format!("Container log — the last {n} lines"),
                        }}
                    </div>
                    <span class="spacer"></span>
                    <label class="dim mono-sm" for="svc-log-lines">
                        "lines (0 = all)"
                    </label>
                    <input
                        id="svc-log-lines"
                        class="input mono-sm w-num"
                        type="number"
                        min="0"
                        prop:value=move || log_lines.get()
                        on:input=move |ev| log_lines.set(event_target_value(&ev))
                    />
                    <button class="btn ghost sm" disabled=move || log_busy.get() on:click=fetch_log.clone()>
                        "Read"
                    </button>
                </div>
                <pre class="mono-sm run-log">
                    {move || {
                        let t = log.get();
                        if t.trim().is_empty() {
                            "(nothing yet — the container has written no log)".to_string()
                        } else {
                            t
                        }
                    }}
                </pre>
            </div>
        </Show>

        // Loading the frame is what starts the container, and a disabled
        // agent's cannot start: the frame would only hold the gateway's JSON
        // refusal (ux:U-22). Its dev_url, when set, is served regardless.
        {if enabled || dev_active {
            view! {
                <div class="fill-pane app-frame">
                    <iframe src=src title="the agent's app"></iframe>
                </div>
            }
                .into_any()
        } else {
            view! {
                <div class="fill-pane">
                    <div class="empty">
                        "This agent is disabled, so its app does not start. Enable it on "
                        <a href="/agents">"Agents"</a>
                        " to open it here."
                    </div>
                </div>
            }
                .into_any()
        }}
    }
    .into_any()
}
