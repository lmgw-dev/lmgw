//! Settings — the prose notes each group carries.

use super::*;

// ---------------------------------------------------------------------------
// The prose: every note the old cards carried, folded to its first sentence
// ---------------------------------------------------------------------------

pub(super) fn explain(group: &'static str, page: Page) -> Option<AnyView> {
    let hidden = move || {
        page.searching()
            && !fields()
                .filter(|d| d.group == group)
                .any(|d| page.matches(d))
    };
    let (persist, summary, body): (&'static str, String, AnyView) = match group {
        "responses" => (
            "settings.explain.responses",
            "Backs previous_response_id and MCP tool-approval round trips.".into(),
            view! {
                "Eviction is chain-aware: the whole conversation goes at once, timed from its "
                "most recent response — see "
                <a href="/traffic/conversations">"Traffic → Conversations"</a>
                " for the stored list and a manual evict/clear."
            }
            .into_any(),
        ),
        "money" => (
            "settings.explain.money",
            "The global budget is the ceiling on this gateway rather than on a credential.".into(),
            view! {
                "So it applies even with " <b>"require gateway API keys"</b>
                " off. Crossing it answers " <code>"403 key_budget"</code>
                " — not a 429, because a monthly budget will not clear inside any retry window. "
                "Catalog prices arrive in USD. Changing the currency relabels every amount on the "
                "Usage page and converts nothing — type EUR only if the numbers in your price "
                "sheets are euros."
            }
            .into_any(),
        ),
        "compare" => (
            "settings.explain.compare",
            "The alias whose price answers what your locally-served tokens would have cost in the cloud.".into(),
            view! {
                "Empty leaves the Local vs cloud panel on " <a href="/usage">"Usage"</a>
                " saying no reference is configured, rather than quietly picking one."
            }
            .into_any(),
        ),
        "admission" => (
            "settings.explain.gpu",
            "lmgw is the only ingress for all three containers, so it decides whether a model will fit before the request goes out.".into(),
            view! {
                "It evicts the least recently used idle model when it has to, never one that is "
                "still generating. Both routers run with --models-max 0 so their own count-based "
                "eviction stays out of it. The live ledger and the queue are on "
                <a href="/traffic">"Traffic"</a> ". "
                "The estimate is weights + KV cache + compute buffers, derived from the GGUF "
                "and the row's slots, batch sizes, context and speculative decoding, a chat "
                "model's MTP or draft context included. Not derived, and so not in it: the "
                "driver's own context, allocator slack and a projector's compute buffer. "
                "Headroom is that remainder. Telemetry comes from NVML on NVIDIA and from "
                "amdgpu's sysfs counters on AMD; where neither answers there is nothing to "
                "measure, so set a budget to plan against estimates alone, or leave it at 0 and "
                "admission stays inactive, which the ledger says out loud."
            }
            .into_any(),
        ),
        "hold" => (
            "settings.explain.hold",
            "A manual switch that takes lmgw off the GPU without quitting it — for gaming on the same card.".into(),
            view! {
                "While it is on, a held model answers with its own fallback, the one set in its "
                "editor on the " <a href="/models">"Models"</a> " page — a model of the same "
                "type, which may have a fallback of its own; there is no global one. Unattended "
                "batch jobs (quickdoc ingest, golden-query generation) never fall back: they are "
                "refused and simply re-run after release. A held model with no usable fallback "
                "answers with a 503, error code " <code>"gpu_hold"</code> ", naming the models "
                "the request was handed along."
            }
            .into_any(),
        ),
        "containers" => (
            "settings.explain.containers",
            "Every model runs in its own container, named and ported automatically.".into(),
            view! {
                "A class's settings below are the defaults each of its models inherits; they "
                "reach a running model only when it restarts, or at once with Apply to running "
                "containers. A request timeout is how long one call may take once the model is "
                "up — not the start, which has its own limit under Runtime — and 0 means no "
                "ceiling at all. "
                "Every per-model container is named " <code>"<prefix>-<class>-<slug>-<hash6>"</code>
                " — a dev instance pointed at the same podman socket as prod needs its own "
                "prefix (e.g. " <code>"lmgw-dev"</code>
                "), or the two collide on container names and one instance's start/stop steps on "
                "the other's containers. It is also what scopes agent containers: boot "
                "reconciliation removes every container labelled " <code>"lmgw.kind=agent"</code>
                " with this prefix whose run is not a live job of " <i>"this"</i>
                " instance — so a second lmgw started with the same prefix will kill the first "
                "one's running agents, and their run directories under "
                <code>"$XDG_RUNTIME_DIR/lmgw/<prefix>/"</code> " with them."
            }
            .into_any(),
        ),
        "audio" => (
            "settings.explain.audio",
            "Busy timeout, idle unload, min free memory and max request body go into every model's server.json.".into(),
            view! {
                "Busy timeout bounds the wait for a model that is already running (a second "
                "request fails with 503 rather than parking forever behind a wedged GPU call) — "
                "raise it above the slowest generation this class does, or set a per-model value. "
                "Idle unload frees the model's VRAM without stopping its container; the next "
                "request reloads it. Min free memory refuses a load that would not leave that "
                "much free. The voice library is a directory of <name>.wav clips plus a "
                "prompt_text file, as the container sees it: /models/voices is the Audio lab's "
                "own library."
            }
            .into_any(),
        ),
        "image" => (
            "settings.explain.image",
            "Saving a models dir creates loras/ and upscalers/ inside it.".into(),
            view! {
                "sd-server's capabilities route throws when its LoRA and upscaler flags point at "
                "nothing, and lmgw renders both on every start. sd-server has no config file, so "
                "everything per-process is a flag in some model's args."
            }
            .into_any(),
        ),
        "forge" => (
            "settings.explain.forge",
            "A token goes only to its own host: in forge API calls, and as a git header passed through the environment, never argv or a log.".into(),
            view! {
                "The Engines page uses it to list pull requests for the extras picker and to "
                "check builds for new commits. Without one, GitHub answers 60 API requests an "
                "hour per address; with one, 5,000. A token that can read the repositories you "
                "build is enough. Other forges (GitLab, Gitea, Forgejo) are named by host alone, "
                "with a port when it is not 443: " <code>"git.example:8443"</code> ". "
                "The self-admin tools can neither read nor write these."
            }
            .into_any(),
        ),
        "agent-containers" => (
            "settings.explain.agents",
            "Agent UIs live at http://<id>.<suffix>:<port>/.".into(),
            view! {
                "Change the suffix only for a gateway reached from other machines, to a zone with "
                "a wildcard record that is not a sibling of the dashboard's own name — a page "
                "under the suffix could set a cookie on the shared parent domain and overwrite "
                "your session. Changing it stops every running app container. "
                "An agent step declared as a " <code>"script"</code>
                " runs in the agent script image — lmgw mounts its own shim and the manifest's "
                "module read-only and starts " <code>"node /lmgw/shim.mjs"</code>
                " under the same limits, cancel and run log every container agent gets. It is "
                "pulled on first use if it is not on the box; pin a digest here for an "
                "air-gapped or reproducible install."
            }
            .into_any(),
        ),
        "ingest" => (
            "settings.explain.docs",
            "Ingestion sizes each extraction window as the ingest model's real context minus the prompt minus the reply budget.".into(),
            view! {
                "The reply budget is the one number in that sum that is a choice, so it lives "
                "here. Corpora, the request queue and the search playground are on "
                <a href="/docs">"Docs"</a> "."
            }
            .into_any(),
        ),
        "window" if crate::ui_scale::supported() => (
            "settings.explain.appearance",
            "Zooms the whole window — text, controls, charts and spacing together.".into(),
            view! {
                "Ctrl + and Ctrl - step it, Ctrl 0 returns to 100%, Ctrl+wheel does the same. It "
                "is remembered per window, not a gateway setting — the theme too."
            }
            .into_any(),
        ),
        "window" => (
            "settings.explain.appearance",
            "This is the app window's own zoom, so it only applies inside the lmgw shell.".into(),
            view! {
                "In a browser tab use the browser's zoom (Ctrl +/-), which does exactly the same "
                "thing and is remembered per site. The theme is remembered per window too, not as "
                "a gateway setting."
            }
            .into_any(),
        ),
        other => realtime::explain(other)?,
    };
    // Why the stored suffix shadows the gateway, when it does: said where the
    // setting is read, not only where it was typed (origins §4.1).
    // And that agent pages are unreachable from other machines under the
    // `localhost` suffix where other machines reach the dashboard (headless
    // server design D27).
    let warning = (group == "agent-containers").then_some(move || {
        page.data.with(|d| {
            let v = d.as_ref()?;
            let lines: Vec<_> = ["agent_origin_suffix_warning", "agent_origin_remote_warning"]
                .iter()
                .filter_map(|k| v[*k].as_str())
                .map(|w| view! { <div class="notice warn">{w.to_string()}</div> })
                .collect();
            (!lines.is_empty()).then_some(lines)
        })
    });
    Some(
        view! {
            {warning}
            <div class="set-explain" hidden=hidden>
                <Explain summary=summary persist=persist>
                    {body}
                </Explain>
            </div>
        }
        .into_any(),
    )
}
