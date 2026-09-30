//! The build editor's extras picker (container-builds design §4 "extras",
//! §7, §9.1): the ordered list of what is merged on top of the ref, and the
//! three ways to add to it — the forge's open PRs/MRs (searchable, paged),
//! a number or pasted URL, and a ref from any other remote.

use std::collections::HashMap;
use std::time::Duration;

use leptos::prelude::*;
use leptos::task::spawn_local;
use lmgw_api_types::builds::{
    BuildExtra, Forge, ForgePr, ForgePrArgs, ForgePrsArgs, RateLimit, RemoteRefsView,
};

use super::{ago, local_ts, sha7, short_repo, use_bk};
use crate::backends_api as api;
use crate::scope::Scope;
use crate::widgets::ShowMore;

/// `(host, path)` of a git URL, lowercased and without `.git`, so two
/// spellings of one repository compare equal: `https://github.com/o/r`,
/// `https://github.com/O/r.git/`, `git@github.com:o/r`, `ssh://git@github.com/o/r`.
pub fn repo_key(url: &str) -> Option<(String, String)> {
    let u = url.trim();
    let (host, path) = match u.split_once("://") {
        Some((_, rest)) => {
            let (host, path) = rest.split_once('/')?;
            let host = host.rsplit_once('@').map_or(host, |(_, h)| h);
            let host = host.split_once(':').map_or(host, |(h, _)| h);
            (host, path)
        }
        None => {
            let (userhost, path) = u.split_once(':')?;
            let host = userhost.rsplit_once('@').map_or(userhost, |(_, h)| h);
            (host, path)
        }
    };
    let path = path.trim_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    if host.is_empty() || path.is_empty() {
        return None;
    }
    Some((host.to_lowercase(), path.to_lowercase()))
}

/// What "Add by number or URL" makes of its input. A number (`123`, `#123`,
/// `!123`) is a PR/MR of the build's own repository. A PR or MR URL of the
/// same repository is too; one of **another** repository becomes a ref extra
/// on that remote (`refs/pull/N/head`, `refs/merge-requests/N/head`) — an
/// upstream llama.cpp PR on top of ik is exactly that.
pub fn parse_pr_input(input: &str, repo_url: &str, forge: Forge) -> Result<BuildExtra, String> {
    let s = input.trim();
    if s.is_empty() {
        return Err("type a PR number or paste its URL".to_string());
    }
    let digits = s.trim_start_matches(['#', '!']);
    if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) {
        let number: u64 = digits
            .parse()
            .map_err(|_| format!("'{digits}' is not a PR number"))?;
        if number == 0 {
            return Err("a PR number starts at 1".to_string());
        }
        if forge == Forge::Plain {
            return Err(
                "a plain repository has no pull requests — pick its forge above, or add the \
                 branch as a ref from another remote"
                    .to_string(),
            );
        }
        return Ok(BuildExtra::Pr { number, pin: None });
    }
    let Some((_, rest)) = s.split_once("://") else {
        return Err(format!(
            "'{s}' is neither a PR number nor a pull/merge request URL"
        ));
    };
    let (host, path) = rest
        .split_once('/')
        .ok_or_else(|| format!("'{s}' names no path"))?;
    let segs: Vec<&str> = path
        .split(['?', '#'])
        .next()
        .unwrap_or("")
        .split('/')
        .filter(|p| !p.is_empty())
        .collect();
    let (repo, n, github) = if let Some(i) = segs.iter().position(|p| *p == "pull" || *p == "pulls")
    {
        (segs[..i].join("/"), segs.get(i + 1), true)
    } else if let Some(i) = segs.iter().position(|p| *p == "merge_requests") {
        // GitLab: <group>/<repo>/-/merge_requests/<n>
        let end = if i > 0 && segs[i - 1] == "-" {
            i - 1
        } else {
            i
        };
        (segs[..end].join("/"), segs.get(i + 1), false)
    } else {
        return Err(format!("'{s}' is not a pull request or merge request URL"));
    };
    let number: u64 = n
        .and_then(|n| n.parse().ok())
        .filter(|n| *n > 0)
        .ok_or_else(|| format!("'{s}' carries no PR number"))?;
    if repo.is_empty() {
        return Err(format!("'{s}' names no repository"));
    }
    let same = repo_key(repo_url)
        == Some((
            host.to_lowercase(),
            repo.trim_end_matches(".git").to_lowercase(),
        ));
    if same && forge != Forge::Plain {
        return Ok(BuildExtra::Pr { number, pin: None });
    }
    let git_ref = if github {
        format!("refs/pull/{number}/head")
    } else {
        format!("refs/merge-requests/{number}/head")
    };
    Ok(BuildExtra::Ref {
        remote_url: format!("https://{host}/{repo}"),
        git_ref,
        pin: None,
    })
}

/// Is it a full 40-hex commit SHA?
pub fn is_full_sha(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// The commit a branch or tag name points at in a `forge_refs` answer.
/// Names are compared bare and with their `refs/heads/`/`refs/tags/` prefix,
/// whichever way either side spells them.
pub fn find_ref_sha(v: &RemoteRefsView, git_ref: &str) -> Option<String> {
    let bare = |n: &str| -> String {
        n.strip_prefix("refs/heads/")
            .or_else(|| n.strip_prefix("refs/tags/"))
            .unwrap_or(n)
            .to_string()
    };
    let want = bare(git_ref);
    v.heads
        .iter()
        .chain(v.tags.iter())
        .find(|r| bare(&r.name) == want)
        .map(|r| r.sha.clone())
}

/// A PR's state as a badge: `(class, label)`.
pub fn pr_state(p: &ForgePr) -> (&'static str, String) {
    if p.merged_at.is_some() {
        ("chip off", "merged".to_string())
    } else if p.state != "open" && p.state != "opened" && !p.state.is_empty() {
        ("chip err", p.state.clone())
    } else if p.draft {
        ("chip warn", "draft".to_string())
    } else {
        ("chip ok", "open".to_string())
    }
}

/// How long the PR search waits after the last keystroke.
const DEBOUNCE: Duration = Duration::from_millis(350);
/// PR rows shown before "Show all", out of the page the forge returned.
const PRS_SHOWN: usize = 10;

#[component]
pub fn ExtrasPicker(
    extras: RwSignal<Vec<BuildExtra>>,
    repo_url: RwSignal<String>,
    forge: RwSignal<String>,
    /// PR details by number, filled from the forge as they come in; the
    /// chosen list reads its titles and states from here.
    known: RwSignal<HashMap<u64, ForgePr>>,
    #[prop(into)] error: Signal<Option<String>>,
    /// A button changed the list: the modal counts it as an edit.
    on_change: Callback<()>,
) -> impl IntoView {
    let bk = use_bk();
    let toasts = bk.toasts;
    // The forge lookups below answer after the editor may have closed; its
    // `on_change` is gone with it, and calling a disposed callback traps the
    // module. Made beside it, so its tasks end with the editor.
    let scope = Scope::new();
    let has_forge = move || Forge::parse(&forge.get()).unwrap_or_default() != Forge::Plain;
    // For handlers: what the form says right now, without subscribing.
    let forge_v = move || Forge::parse(&forge.get_untracked()).unwrap_or_default();

    let changed = move || on_change.run(());
    let add = move |e: BuildExtra| -> Result<(), String> {
        let dup = extras.with_untracked(|list| {
            list.iter().any(|x| match (x, &e) {
                (BuildExtra::Pr { number: a, .. }, BuildExtra::Pr { number: b, .. }) => a == b,
                (
                    BuildExtra::Ref {
                        remote_url: ra,
                        git_ref: fa,
                        ..
                    },
                    BuildExtra::Ref {
                        remote_url: rb,
                        git_ref: fb,
                        ..
                    },
                ) => repo_key(ra) == repo_key(rb) && fa == fb,
                _ => false,
            })
        });
        if dup {
            return Err(format!("{} is already in the list", e.label()));
        }
        extras.update(|l| l.push(e));
        changed();
        Ok(())
    };
    // Sets the pin on the entry equal to `target` — found again by value, so
    // an answer that arrives after the list was reordered still lands right.
    let set_pin = move |target: BuildExtra, pin: Option<String>| {
        extras.update(|l| {
            if let Some(x) = l.iter_mut().find(|x| **x == target) {
                match x {
                    BuildExtra::Pr { pin: p, .. } | BuildExtra::Ref { pin: p, .. } => *p = pin,
                }
            }
        });
        changed();
    };
    let pinning = RwSignal::new(false);
    let toggle_pin = move |e: BuildExtra| {
        if e.pin().is_some() {
            set_pin(e, None);
            return;
        }
        match e.clone() {
            BuildExtra::Pr { number, .. } => {
                let head = known.with_untracked(|k| {
                    k.get(&number)
                        .map(|p| p.head_sha.clone())
                        .filter(|s| !s.is_empty())
                });
                if let Some(sha) = head {
                    set_pin(e, Some(sha));
                    return;
                }
                let args = ForgePrArgs {
                    repo_url: repo_url.get_untracked(),
                    forge: forge_v(),
                    number,
                };
                pinning.set(true);
                scope.spawn(async move {
                    let res = api::forge_pr(&args).await;
                    pinning.set(false);
                    match res {
                        Ok(p) if !p.head_sha.is_empty() => {
                            let sha = p.head_sha.clone();
                            known.update(|k| {
                                k.insert(number, p);
                            });
                            set_pin(e, Some(sha));
                        }
                        Ok(_) => {
                            toasts.err(format!("PR #{number}: the forge reported no head commit"))
                        }
                        Err(err) => toasts.err(format!("PR #{number}: {err}")),
                    }
                });
            }
            BuildExtra::Ref {
                remote_url,
                git_ref,
                ..
            } => {
                if is_full_sha(&git_ref) {
                    set_pin(e, Some(git_ref.to_lowercase()));
                    return;
                }
                pinning.set(true);
                scope.spawn(async move {
                    let res = api::forge_refs(remote_url.clone()).await;
                    pinning.set(false);
                    match res {
                        Ok(v) => match find_ref_sha(&v, &git_ref) {
                            Some(sha) => set_pin(e, Some(sha)),
                            None => toasts.err(format!(
                                "{git_ref} is not a branch or tag of {remote_url} — to pin it, \
                                 run Check merge and use the commit it reports, or add the \
                                 commit's full SHA as the ref"
                            )),
                        },
                        Err(err) => toasts.err(format!("{remote_url}: {err}")),
                    }
                });
            }
        }
    };
    let move_by = move |i: usize, down: bool| {
        extras.update(|l| {
            let j = if down { i + 1 } else { i.wrapping_sub(1) };
            if i < l.len() && j < l.len() {
                l.swap(i, j);
            }
        });
        changed();
    };
    let remove = move |i: usize| {
        extras.update(|l| {
            if i < l.len() {
                l.remove(i);
            }
        });
        changed();
    };

    // The details of PRs already on the list, for their titles and states.
    let asked = StoredValue::new(false);
    Effect::new(move |_| {
        if asked.get_value() {
            return;
        }
        asked.set_value(true);
        let f = Forge::parse(&forge.get_untracked()).unwrap_or_default();
        if f == Forge::Plain {
            return;
        }
        let url = repo_url.get_untracked();
        let numbers: Vec<u64> = extras.with_untracked(|l| {
            l.iter()
                .filter_map(|e| match e {
                    BuildExtra::Pr { number, .. } => Some(*number),
                    _ => None,
                })
                .collect()
        });
        if numbers.is_empty() {
            return;
        }
        scope.spawn(async move {
            for number in numbers {
                let args = ForgePrArgs {
                    repo_url: url.clone(),
                    forge: f,
                    number,
                };
                // Best effort: a state badge is extra information, and the
                // rate limit is better spent on the list the owner asks for.
                match api::forge_pr(&args).await {
                    Ok(p) => known.update(|k| {
                        k.insert(number, p);
                    }),
                    Err(_) => break,
                }
            }
        });
    });

    let by_number = RwSignal::new(String::new());
    let by_number_err = RwSignal::new(None::<String>);
    let add_by_number = move || {
        let res = parse_pr_input(
            &by_number.get_untracked(),
            &repo_url.get_untracked(),
            forge_v(),
        )
        .and_then(add);
        match res {
            Ok(()) => {
                by_number.set(String::new());
                by_number_err.set(None);
            }
            Err(e) => by_number_err.set(Some(e)),
        }
    };
    let remote = RwSignal::new(String::new());
    let remote_ref = RwSignal::new(String::new());
    let remote_err = RwSignal::new(None::<String>);
    let add_remote = move || {
        let url = remote.get_untracked().trim().to_string();
        let r = remote_ref.get_untracked().trim().to_string();
        if url.is_empty() || r.is_empty() {
            remote_err.set(Some(
                "name the remote's URL and the branch, tag or commit".into(),
            ));
            return;
        }
        match add(BuildExtra::Ref {
            remote_url: url,
            git_ref: r,
            pin: None,
        }) {
            Ok(()) => {
                remote.set(String::new());
                remote_ref.set(String::new());
                remote_err.set(None);
            }
            Err(e) => remote_err.set(Some(e)),
        }
    };

    view! {
        <p class="notice warn bk-pr-note">
            "Building a PR runs that PR's build code: its Dockerfile and CMake files run on this "
            "machine. Add only what you would run yourself."
        </p>
        <div class="bk-extras">
            {move || {
                let list = extras.get();
                if list.is_empty() {
                    return view! {
                        <div class="dim">"No extras — the build is the ref as it is."</div>
                    }
                        .into_any();
                }
                let n = list.len();
                list.into_iter()
                    .enumerate()
                    .map(|(i, e)| {
                        let (label, title, state) = match &e {
                            BuildExtra::Pr { number, .. } => {
                                known
                                    .with(|k| k.get(number).cloned())
                                    .map(|p| {
                                        let (c, l) = pr_state(&p);
                                        (
                                            format!("#{number} {}", p.title),
                                            format!("#{number} {}\nby {} · {}", p.title, p.author, p.url),
                                            Some((c, l)),
                                        )
                                    })
                                    .unwrap_or_else(|| (format!("PR #{number}"), e.label(), None))
                            }
                            BuildExtra::Ref { remote_url, git_ref, .. } => {
                                (
                                    format!("{} {git_ref}", short_repo(remote_url)),
                                    format!("{remote_url}\n{git_ref}"),
                                    None,
                                )
                            }
                        };
                        let pin = e.pin().map(sha7);
                        let pinned = pin.is_some();
                        let pin_title = match e.pin() {
                            Some(p) => format!("Pinned to {p}: every run builds exactly this commit. Unpin to follow the head again."),
                            None => "Follows the head: each run merges whatever it points at then. Pin to build this exact commit.".to_string(),
                        };
                        let e_pin = e.clone();
                        view! {
                            <div class="bk-extra">
                                <span class="dim mono-sm">{format!("{}.", i + 1)}</span>
                                <span class="bk-x-label" title=title>{label}</span>
                                {state.map(|(c, l)| view! { <span class=c><span class="dot"></span>{l}</span> })}
                                <span class="mono-sm dim" title=pin_title.clone()>
                                    {pin.clone().unwrap_or_else(|| "follows head".to_string())}
                                </span>
                                <button
                                    type="button"
                                    class="btn ghost sm"
                                    title=pin_title
                                    disabled=move || pinning.get()
                                    on:click=move |_| toggle_pin(e_pin.clone())
                                >
                                    {if pinned { "Unpin" } else { "Pin" }}
                                </button>
                                <button
                                    type="button"
                                    class="btn ghost sm"
                                    title="Merge it earlier"
                                    disabled=i == 0
                                    on:click=move |_| move_by(i, false)
                                >
                                    "↑"
                                </button>
                                <button
                                    type="button"
                                    class="btn ghost sm"
                                    title="Merge it later"
                                    disabled=i + 1 == n
                                    on:click=move |_| move_by(i, true)
                                >
                                    "↓"
                                </button>
                                <button
                                    type="button"
                                    class="btn ghost sm"
                                    title="Take it off the list"
                                    on:click=move |_| remove(i)
                                >
                                    "✕"
                                </button>
                            </div>
                        }
                    })
                    .collect_view()
                    .into_any()
            }}
            {move || error.get().map(|e| view! { <div class="field-err" role="alert">{e}</div> })}
        </div>

        <div class="field-grid bk-add">
            <div class="field wide">
                <label>"Add by number or URL"</label>
                <div class="row bk-inline">
                    <input
                        class="input mono spacer"
                        placeholder="16391, #16391, or a pull/merge request URL"
                        prop:value=move || by_number.get()
                        on:input=move |ev| {
                            by_number.set(event_target_value(&ev));
                            by_number_err.set(None);
                        }
                        on:keydown=move |ev| {
                            if ev.key() == "Enter" {
                                ev.prevent_default();
                                add_by_number();
                            }
                        }
                    />
                    <button
                        type="button"
                        class="btn"
                        disabled=move || by_number.with(|v| v.trim().is_empty())
                        on:click=move |_| add_by_number()
                    >
                        "Add"
                    </button>
                </div>
                {move || by_number_err.get().map(|e| view! { <div class="field-err" role="alert">{e}</div> })}
            </div>
            <div class="field wide">
                <label>
                    "Add a ref from another remote"
                    <span class="field-unit">"a fork's branch, a tag, a commit, refs/pull/N/head"</span>
                </label>
                <div class="row bk-inline">
                    <input
                        class="input mono spacer"
                        placeholder="https://github.com/someone/llama.cpp"
                        prop:value=move || remote.get()
                        on:input=move |ev| {
                            remote.set(event_target_value(&ev));
                            remote_err.set(None);
                        }
                    />
                    <input
                        class="input mono w-sm"
                        placeholder="branch, tag or SHA"
                        prop:value=move || remote_ref.get()
                        on:input=move |ev| {
                            remote_ref.set(event_target_value(&ev));
                            remote_err.set(None);
                        }
                        on:keydown=move |ev| {
                            if ev.key() == "Enter" {
                                ev.prevent_default();
                                add_remote();
                            }
                        }
                    />
                    <button type="button" class="btn" on:click=move |_| add_remote()>
                        "Add"
                    </button>
                </div>
                {move || remote_err.get().map(|e| view! { <div class="field-err" role="alert">{e}</div> })}
            </div>
        </div>

        <Show
            when=has_forge
            fallback=|| {
                view! {
                    <p class="dim mini-note">
                        "A plain git repository has no pull request list: add refs from other remotes."
                    </p>
                }
            }
        >
            <PrBrowser repo_url=repo_url forge=forge known=known extras=extras add=Callback::new(move |e: BuildExtra| {
                if let Err(msg) = add(e) {
                    toasts.warn(msg);
                }
            })/>
        </Show>
    }
}

/// The forge's open PRs/MRs: searched server-side (debounced), a page at a
/// time, with the rate limit said when it bites.
#[component]
fn PrBrowser(
    repo_url: RwSignal<String>,
    forge: RwSignal<String>,
    known: RwSignal<HashMap<u64, ForgePr>>,
    extras: RwSignal<Vec<BuildExtra>>,
    add: Callback<BuildExtra>,
) -> impl IntoView {
    let open = RwSignal::new(false);
    let query = RwSignal::new(String::new());
    let prs = RwSignal::new(Vec::<ForgePr>::new());
    let next = RwSignal::new(None::<u32>);
    let rate = RwSignal::new(None::<RateLimit>);
    let err = RwSignal::new(None::<String>);
    let loading = RwSignal::new(false);
    let fetched = RwSignal::new(false);
    let generation = StoredValue::new(0u64);
    let timer = StoredValue::new(None::<TimeoutHandle>);
    let shown = RwSignal::new(PRS_SHOWN);

    let fetch = move |page: Option<u32>| {
        let g = generation.get_value() + 1;
        generation.set_value(g);
        let q = query.get_untracked().trim().to_string();
        let args = ForgePrsArgs {
            repo_url: repo_url.get_untracked(),
            forge: Forge::parse(&forge.get_untracked()).unwrap_or_default(),
            query: (!q.is_empty()).then_some(q),
            page,
        };
        loading.set(true);
        spawn_local(async move {
            let res = api::forge_prs(&args).await;
            // A newer search was started meanwhile: this answer is stale.
            if generation.try_get_value() != Some(g) {
                return;
            }
            loading.set(false);
            fetched.set(true);
            match res {
                Ok(p) => {
                    err.set(None);
                    rate.set(p.rate_limit.clone());
                    next.set(p.next_page);
                    known.update(|k| {
                        for pr in &p.prs {
                            k.insert(pr.number, pr.clone());
                        }
                    });
                    if page.is_none() {
                        prs.set(p.prs);
                    } else {
                        prs.update(|v| v.extend(p.prs));
                    }
                }
                Err(e) => err.set(Some(e.to_string())),
            }
        });
    };
    // Another repository or forge: what was listed is someone else's.
    Effect::new(move |prev: Option<(String, String)>| {
        let now = (repo_url.get(), forge.get());
        if prev.is_some_and(|p| p != now) {
            prs.set(Vec::new());
            next.set(None);
            fetched.set(false);
            if open.get_untracked() {
                fetch(None);
            }
        }
        now
    });
    on_cleanup(move || {
        if let Some(h) = timer.try_get_value().flatten() {
            h.clear();
        }
    });
    let on_query = move |v: String| {
        query.set(v);
        if let Some(h) = timer.get_value() {
            h.clear();
        }
        let h = set_timeout_with_handle(move || fetch(None), DEBOUNCE).ok();
        timer.set_value(h);
    };
    let toggle = move |_| {
        let now = !open.get_untracked();
        open.set(now);
        if now && !fetched.get_untracked() {
            fetch(None);
        }
    };
    let added = move |n: u64| {
        extras.with(|l| {
            l.iter()
                .any(|e| matches!(e, BuildExtra::Pr { number, .. } if *number == n))
        })
    };
    let total = Signal::derive(move || prs.with(Vec::len));
    let forge_more = move || next.with(Option::is_some) && shown.get() >= prs.with(Vec::len);
    let shown_prs = move || {
        let n = shown.get();
        prs.get().into_iter().take(n).collect::<Vec<_>>()
    };
    let limit_line = move || {
        let r = rate.get()?;
        let reset = local_ts(&r.reset_at);
        if r.remaining == 0 {
            Some(
                view! {
                    <div class="notice warn">
                        <b>"The forge's API limit is used up"</b>
                        {format!(
                            "It resets at {reset}. Adding a PR by number still works meanwhile; a forge token for this host lifts the limit: "
                        )}
                        <a href=crate::pages::settings_href("forge_tokens")>
                            "Settings → Backends → Forge tokens"
                        </a>
                    </div>
                }
                    .into_any(),
            )
        } else if r.remaining < 10 {
            Some(
                view! {
                    <div class="dim mini-note">
                        {format!("{} forge API requests left until {reset}", r.remaining)}
                    </div>
                }
                .into_any(),
            )
        } else {
            None
        }
    };
    view! {
        <div class="bk-prs">
            <button
                type="button"
                class="link-btn bk-toggle"
                aria-expanded=move || open.get().to_string()
                on:click=toggle
            >
                <span class="caret-icon" aria-hidden="true">"▸"</span>
                "Browse open pull requests"
            </button>
            <Show when=move || open.get()>
                <div class="row bk-inline" style="margin-top:8px">
                    <input
                        class="input spacer"
                        type="search"
                        placeholder="Search titles"
                        data-untracked
                        prop:value=move || query.get()
                        on:input=move |ev| on_query(event_target_value(&ev))
                    />
                    <span class="dim mono-sm">
                        {move || if loading.get() { "loading…".to_string() } else { String::new() }}
                    </span>
                </div>
                {limit_line}
                {move || {
                    err.get()
                        .map(|e| {
                            let limited = e.to_lowercase().contains("rate limit");
                            view! {
                                <div class=if limited { "notice warn row" } else { "notice err row" }>
                                    <span class="spacer">
                                        {e}
                                        {limited
                                            .then(|| {
                                                view! {
                                                    " A forge token lifts the limit: "
                                                    <a href=crate::pages::settings_href("forge_tokens")>
                                                        "Settings → Backends"
                                                    </a>
                                                }
                                            })}
                                    </span>
                                    <button class="btn ghost sm" on:click=move |_| fetch(None)>
                                        "Retry"
                                    </button>
                                </div>
                            }
                        })
                }}
                <div class="bk-pr-list">
                    <For
                        each=shown_prs
                        key=|p| (p.number, p.updated_at.clone(), p.state.clone())
                        let:p
                    >
                        {
                            let n = p.number;
                            let (c, l) = pr_state(&p);
                            let title = format!("#{n} {}\nby {} · updated {}\n{}", p.title, p.author, local_ts(&p.updated_at), p.url);
                            view! {
                                <div class="bk-pr" title=title>
                                    <span class="mono-sm">{format!("#{n}")}</span>
                                    <span class="bk-x-label">{p.title.clone()}</span>
                                    <span class="dim bk-pr-meta">{p.author.clone()}</span>
                                    <span class="dim bk-pr-meta">{ago(&p.updated_at)}</span>
                                    <span class=c><span class="dot"></span>{l}</span>
                                    <button
                                        type="button"
                                        class="btn sm"
                                        disabled=move || added(n)
                                        on:click=move |_| add.run(BuildExtra::Pr { number: n, pin: None })
                                    >
                                        {move || if added(n) { "Added" } else { "Add" }}
                                    </button>
                                </div>
                            }
                        }
                    </For>
                    <Show when=move || fetched.get() && prs.with(Vec::is_empty) && err.with(Option::is_none)>
                        <div class="dim">
                            {move || if query.with(|q| q.trim().is_empty()) {
                                "No open pull requests.".to_string()
                            } else {
                                format!("No open pull request matches \u{201c}{}\u{201d}.", query.get().trim())
                            }}
                        </div>
                    </Show>
                </div>
                <ShowMore total=total shown=shown step=PRS_SHOWN noun="pull requests"/>
                // The forge's next page once every loaded row is shown: two
                // "more" lines at once read as one list shown two ways.
                <Show when=forge_more>
                    <div class="show-more">
                        <span>{move || format!("{} loaded", prs.with(Vec::len))}</span>
                        <button
                            type="button"
                            class="link-btn"
                            disabled=move || loading.get()
                            on:click=move |_| {
                                shown.set(usize::MAX);
                                fetch(next.get_untracked());
                            }
                        >
                            "Load more from the forge"
                        </button>
                    </div>
                </Show>
            </Show>
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lmgw_api_types::builds::RefEntry;

    const REPO: &str = "https://github.com/ggml-org/llama.cpp";

    #[test]
    fn one_repository_spelled_many_ways_is_one_key() {
        let k = Some(("github.com".to_string(), "ggml-org/llama.cpp".to_string()));
        assert_eq!(repo_key(REPO), k);
        assert_eq!(repo_key("https://github.com/GGML-org/llama.cpp.git/"), k);
        assert_eq!(repo_key("git@github.com:ggml-org/llama.cpp.git"), k);
        assert_eq!(repo_key("ssh://git@github.com:22/ggml-org/llama.cpp"), k);
        assert_eq!(repo_key("nonsense"), None);
    }

    #[test]
    fn numbers_are_prs_of_the_builds_own_repository() {
        for s in ["16391", "#16391", "!16391", " 16391 "] {
            assert_eq!(
                parse_pr_input(s, REPO, Forge::Github),
                Ok(BuildExtra::Pr {
                    number: 16391,
                    pin: None
                }),
                "{s}"
            );
        }
        assert!(parse_pr_input("0", REPO, Forge::Github).is_err());
        assert!(parse_pr_input("12", REPO, Forge::Plain).is_err());
        assert!(parse_pr_input("", REPO, Forge::Github).is_err());
        assert!(parse_pr_input("feature-x", REPO, Forge::Github).is_err());
    }

    #[test]
    fn a_url_of_the_same_repository_is_a_pr_and_of_another_a_ref() {
        assert_eq!(
            parse_pr_input(
                "https://github.com/ggml-org/llama.cpp/pull/16391/files",
                REPO,
                Forge::Github
            ),
            Ok(BuildExtra::Pr {
                number: 16391,
                pin: None
            })
        );
        // an upstream llama.cpp PR on top of ik
        assert_eq!(
            parse_pr_input(
                "https://github.com/ggml-org/llama.cpp/pull/16391",
                "https://github.com/ikawrakow/ik_llama.cpp",
                Forge::Github
            ),
            Ok(BuildExtra::Ref {
                remote_url: "https://github.com/ggml-org/llama.cpp".into(),
                git_ref: "refs/pull/16391/head".into(),
                pin: None,
            })
        );
        // a GitLab MR
        assert_eq!(
            parse_pr_input(
                "https://git.example.com/alice/llama.cpp/-/merge_requests/7",
                "https://git.example.com/alice/llama.cpp.git",
                Forge::Gitlab
            ),
            Ok(BuildExtra::Pr {
                number: 7,
                pin: None
            })
        );
        assert_eq!(
            parse_pr_input(
                "https://git.example.com/other/llama.cpp/-/merge_requests/7",
                REPO,
                Forge::Github
            ),
            Ok(BuildExtra::Ref {
                remote_url: "https://git.example.com/other/llama.cpp".into(),
                git_ref: "refs/merge-requests/7/head".into(),
                pin: None,
            })
        );
        assert!(parse_pr_input("https://github.com/o/r/issues/3", REPO, Forge::Github).is_err());
    }

    #[test]
    fn a_ref_is_found_bare_or_prefixed() {
        let v = RemoteRefsView {
            default_branch: "master".into(),
            heads: vec![RefEntry {
                name: "refs/heads/feature-x".into(),
                sha: "a".repeat(40),
            }],
            tags: vec![RefEntry {
                name: "b6000".into(),
                sha: "b".repeat(40),
            }],
        };
        assert_eq!(find_ref_sha(&v, "feature-x"), Some("a".repeat(40)));
        assert_eq!(find_ref_sha(&v, "refs/tags/b6000"), Some("b".repeat(40)));
        assert_eq!(find_ref_sha(&v, "refs/pull/1/head"), None);
    }

    #[test]
    fn only_a_full_sha_is_one() {
        assert!(is_full_sha(&"0123456789abcdef".repeat(3)[..40]));
        assert!(!is_full_sha("0123456"));
        assert!(!is_full_sha(&"g".repeat(40)));
    }

    #[test]
    fn a_pr_state_badge_prefers_merged_then_closed_then_draft() {
        let mut p = ForgePr {
            state: "open".into(),
            ..Default::default()
        };
        assert_eq!(pr_state(&p).1, "open");
        p.draft = true;
        assert_eq!(pr_state(&p).1, "draft");
        p.state = "closed".into();
        assert_eq!(pr_state(&p).1, "closed");
        p.merged_at = Some("2026-09-01T00:00:00Z".into());
        assert_eq!(pr_state(&p).1, "merged");
    }
}
