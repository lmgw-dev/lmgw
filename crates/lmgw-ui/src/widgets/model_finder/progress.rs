//! Get it and what follows (find-a-model design §6): `model_get` with the
//! file and the measure the card showed (D120, D24), its progress from the
//! jobs frame on `/api/events` (one line per file with its bytes, the time
//! left, the engine's line, Cancel), the end read with `model_get_status`,
//! and "Say hello" or "Ready for Knowledge" (§6.3, D29).

use leptos::prelude::*;
use lmgw_api_types::model_find::{CardKind, FitMeasure, ModelCard};
use lmgw_api_types::model_get::{
    ModelGetArgs, ModelGetDetail, ModelGetDone, ModelGetStage, ModelGetStarted, ModelGetState,
    ModelGetStatus, ModelGetStatusArgs,
};
use lmgw_api_types::JobRow;
use serde_json::json;

mod track;

use super::shown::Shown;
use super::{now_ms, words, Finder, Getting, ReadyModel};
use crate::fmt::{fmt_bytes_of, fmt_bytes_so_far};
use crate::scope::Scope;
use track::{EndRead, FileLine, FileState, Known, Next};

/// "Get it" (§6.1): the card's file with the measure it was shown by, the
/// card's original naming the id (D84) when the file is from one of its
/// uploads; a file chosen by hand from another repo is named by the server.
pub(super) fn start(f: Finder, card: &ModelCard, s: &Shown) {
    let args = ModelGetArgs {
        repo: s.upload.clone(),
        file: s.file.clone(),
        target: s.target.clone(),
        measure: Some(s.fit.measure),
        model_id: Some(words::model_id_for(&card.origin))
            .filter(|m| !m.is_empty() && of_the_card(card, &s.upload)),
    };
    f.get.set(Getting::Starting);
    f.disk_ask.set(false);
    let gen = f.generation.get_value();
    f.scope.spawn(async move {
        let res = crate::api::post::<ModelGetStarted, _>("/api/op/model_get", &args).await;
        if f.generation.get_value() != gen {
            return;
        }
        match res {
            // A row already serves the file (D87): it is the answer.
            Ok(st) if st.job_id.is_none() => f.get.set(Getting::Done(ModelGetDone {
                model_id: st.model_id,
                public_name: st.public_name,
                target: st.target,
                row_id: st.row_id.unwrap_or_default(),
                created: false,
                ..ModelGetDone::default()
            })),
            Ok(st) => f.get.set(Getting::Running(st)),
            Err(e) => f.get.set(Getting::Failed(e.to_string())),
        }
    });
}

/// Whether `repo` is the card's upload or one of the other uploads its
/// answer listed: a file from it is the card's model.
fn of_the_card(card: &ModelCard, repo: &str) -> bool {
    let same = |r: &str| r.eq_ignore_ascii_case(repo.trim());
    same(&card.upload) || card.other_uploads.iter().any(|u| same(&u.repo))
}

/// The rate the download moves at: from its own progress once it has moved
/// for a few seconds, else the card's estimate (D23).
fn measured_rate(first: Option<(f64, u64)>, now: (f64, u64), fallback: Option<f64>) -> Option<f64> {
    match first {
        Some((t0, d0)) if now.0 - t0 >= 3_000.0 && now.1 > d0 => {
            Some((now.1 - d0) as f64 / ((now.0 - t0) / 1000.0))
        }
        _ => fallback,
    }
}

#[component]
pub(super) fn GetProgress(f: Finder, started: ModelGetStarted, rate: Option<f64>) -> impl IntoView {
    let live = crate::live::use_live();
    // The progress's own reads and timers end with it (a new search, the
    // end shown): none reads what it owns once it is gone.
    let scope = Scope::new();
    let job_id = started.job_id;
    let get_id = started.get_id;
    let downloads = StoredValue::new(started.downloads.clone());
    // Each download row's status, the newest read's: model_get's answer,
    // then each status read's.
    let statuses = RwSignal::new(
        started
            .downloads
            .iter()
            .map(|d| (d.id, d.status.clone()))
            .collect::<Vec<_>>(),
    );
    // Each file's last bytes seen on the feed, so one keeps its size once
    // its transfer left the feed.
    let seen = StoredValue::new(Vec::<(i64, u64, Option<u64>)>::new());
    let first = StoredValue::new(None::<(f64, u64)>);
    let end = StoredValue::new(EndRead::default());
    // Bumped to ask the feed's effect for another status read.
    let again = RwSignal::new(0u32);
    let jobs = Memo::new(move |_| live.jobs.get().unwrap_or_default());
    let get_job = Memo::new(move |_| -> Option<JobRow> {
        jobs.with(|j| j.iter().find(|j| Some(j.id) == job_id).cloned())
    });
    let detail = Memo::new(move |_| {
        get_job.with(|j| {
            j.as_ref()
                .and_then(|j| serde_json::from_value::<ModelGetDetail>(j.detail.clone()).ok())
        })
    });
    // The get's transfers on the feed now: (download row, done, total).
    let transfers = Memo::new(move |_| {
        let now = jobs.with(|js| {
            downloads.with_value(|ds| {
                ds.iter()
                    .filter_map(|d| {
                        let key = format!("hf:{}", d.id);
                        js.iter()
                            .find(|j| j.key.as_deref() == Some(key.as_str()))
                            .map(|j| (d.id, j.done, j.total))
                    })
                    .collect::<Vec<_>>()
            })
        });
        seen.update_value(|s| {
            for (id, done, total) in &now {
                s.retain(|(i, _, _)| i != id);
                s.push((*id, *done, *total));
            }
        });
        now
    });
    let lines = Memo::new(move |_| {
        let now = transfers.get();
        let (paused, waiting) = detail.with(|d| match d {
            Some(d) => (d.stage == ModelGetStage::Paused, d.waiting.clone()),
            None => (false, Vec::new()),
        });
        statuses.with(|st| {
            downloads.with_value(|ds| {
                ds.iter()
                    .map(|d| {
                        let status = st
                            .iter()
                            .find(|(id, _)| *id == d.id)
                            .map_or(d.status.as_str(), |(_, s)| s.as_str());
                        let last = seen.with_value(|s| {
                            s.iter()
                                .find(|(id, _, _)| *id == d.id)
                                .map(|(_, done, total)| (*done, *total))
                        });
                        let live = now
                            .iter()
                            .find(|(id, _, _)| *id == d.id)
                            .map(|(_, done, total)| (*done, *total));
                        track::file_line(
                            &d.file,
                            Known {
                                status,
                                live,
                                last,
                                paused,
                                named: waiting.iter().any(|w| w.file == d.file),
                            },
                        )
                    })
                    .collect::<Vec<_>>()
            })
        })
    });

    // The end (D93): the get's status, read when its job isn't on the feed
    // (it ended, or isn't listed yet) and when a file's transfer left the
    // feed short of its total; one read at a time, and a read wanted while
    // one is out goes out after it.
    let read_status = move || {
        let Some(id) = get_id else { return };
        if !end.try_update_value(EndRead::want).unwrap_or(false) {
            return;
        }
        scope.spawn(async move {
            let res = crate::api::post::<ModelGetStatus, _>(
                "/api/op/model_get_status",
                &ModelGetStatusArgs { get_id: id },
            )
            .await;
            let running = matches!(&res, Ok(st) if st.state == ModelGetState::Running);
            let on_feed = get_job.with_untracked(Option::is_some);
            match end.try_update_value(|e| e.back(running, on_feed)) {
                Some(Next::Now) => again.update(|n| *n += 1),
                Some(Next::Soon) => set_timeout(
                    move || {
                        if scope.alive() {
                            again.update(|n| *n += 1);
                        }
                    },
                    std::time::Duration::from_secs(2),
                ),
                _ => {}
            }
            let Ok(st) = res else { return };
            if !st.downloads.is_empty() {
                statuses.set(
                    st.downloads
                        .iter()
                        .map(|d| (d.id, d.status.clone()))
                        .collect(),
                );
            }
            if !matches!(f.get.get_untracked(), Getting::Running(_)) {
                return;
            }
            match st.state {
                ModelGetState::Running => {}
                ModelGetState::Done => {
                    f.get.set(Getting::Done(st.result.unwrap_or(ModelGetDone {
                        model_id: st.model_id,
                        public_name: st.public_name,
                        target: st.target,
                        row_id: st.row_id.unwrap_or_default(),
                        ..ModelGetDone::default()
                    })))
                }
                ModelGetState::Failed => {
                    f.get.set(Getting::Failed(st.error.unwrap_or_else(|| {
                        "Get it stopped. Get it again to try once more.".into()
                    })))
                }
                ModelGetState::Canceled => f.get.set(Getting::Canceled),
            }
        });
    };
    Effect::new(move |_| {
        // Every frame of the feed, and each read asked again: when the
        // get's job is gone, ask how it ended.
        again.track();
        let gone = jobs.with(|_| get_job.with(Option::is_none));
        if gone {
            read_status();
        }
    });
    Effect::new(move |prev: Option<Vec<i64>>| {
        // A transfer that left the feed short of its total: paused, cut or
        // failed, which its row says.
        let now: Vec<i64> = transfers.with(|t| t.iter().map(|(id, _, _)| *id).collect());
        let short = |id: &i64| {
            seen.with_value(|s| {
                s.iter()
                    .find(|(i, _, _)| i == id)
                    .is_some_and(|(_, d, t)| t.is_none_or(|t| *d < t))
            })
        };
        if prev.is_some_and(|p| p.iter().any(|id| !now.contains(id) && short(id))) {
            read_status();
        }
        now
    });

    let totals = Memo::new(move |_| get_job.with(|j| j.as_ref().map(|j| (j.done, j.total))));
    let left = move || {
        // Only while the files move: setting up and a pause have no time.
        if detail.with(|d| {
            d.as_ref()
                .is_some_and(|d| d.stage != ModelGetStage::Downloading)
        }) {
            return None;
        }
        let (done, total) = totals.get()?;
        let total = total?;
        let now = (now_ms(), done);
        if first.get_value().is_none() {
            first.set_value(Some(now));
        }
        let r = measured_rate(first.get_value(), now, rate);
        words::time_left(total.saturating_sub(done), r)
    };
    let stage = move || {
        let d = detail.get();
        match d.as_ref().map(|d| d.stage) {
            Some(ModelGetStage::SettingUp) => "Setting it up…".to_string(),
            Some(ModelGetStage::Ready) => "Ready.".to_string(),
            Some(ModelGetStage::Paused) => {
                words::paused_line(&d.map(|d| d.waiting).unwrap_or_default())
            }
            _ => match totals.get() {
                Some((done, Some(t))) => format!("Downloading · {}", fmt_bytes_of(done, t)),
                Some((done, None)) => format!("Downloading · {}", fmt_bytes_so_far(done)),
                None => "Waiting for the download to start…".to_string(),
            },
        }
    };
    let cancelling = RwSignal::new(false);
    let toasts = f.toasts;
    let cancel = move |_| {
        let Some(id) = job_id else { return };
        cancelling.set(true);
        // Not the scope's: a failed cancel is said even once the progress
        // is gone; what follows touches the progress only while it is here.
        leptos::task::spawn_local(async move {
            let res = crate::api::post::<serde_json::Value, _>(
                "/api/op/job_cancel",
                &json!({ "id": id }),
            )
            .await;
            if let Err(e) = &res {
                toasts.err(e.to_string());
            }
            if scope.alive() {
                cancelling.set(false);
                if res.is_ok() {
                    read_status();
                }
            }
        });
    };
    let engine = started.engine.clone();
    let disk = started.disk.sentence.clone();
    view! {
        <div class="mf-progress" role="status">
            <p class="mf-stage">
                {stage}
                {move || left().map(|l| view! { <span class="dim">{format!(" · {l}")}</span> })}
            </p>
            <ul class="mf-files">
                <For
                    each=move || lines.with(|ls| ls.iter().map(|l| l.file.clone()).collect::<Vec<_>>())
                    key=String::clone
                    let:file
                >
                    <FileRow line=Memo::new(move |_| lines.with(|ls| ls.iter().find(|l| l.file == file).cloned()))/>
                </For>
            </ul>
            {engine.map(|e| view! {
                <div class="dl-line mf-engine">
                    <crate::gpu_download::PullLine engine=e/>
                </div>
            })}
            {disk.map(|d| view! { <p class="mf-line mf-warn">{d}</p> })}
            <footer class="mf-actions">
                <span class="dim">"It keeps going if you close this page; the model then appears on the Models page."</span>
                <span class="mf-spacer"></span>
                <button class="btn" disabled=move || cancelling.get() || job_id.is_none() on:click=cancel>
                    "Cancel"
                </button>
            </footer>
        </div>
    }
}

/// One file's line: its name and bytes, and its bar while it moves, else
/// its state.
#[component]
fn FileRow(line: Memo<Option<FileLine>>) -> impl IntoView {
    let state = Memo::new(move |_| line.with(|l| l.as_ref().map(|l| l.state)));
    let pct = move || {
        line.with(|l| {
            l.as_ref()
                .and_then(|l| l.total.filter(|t| *t > 0).map(|t| l.done.min(t) * 100 / t))
                .unwrap_or(0)
        })
    };
    let name = move || {
        line.with(|l| {
            l.as_ref()
                .map(|l| words::file_progress(&l.file, l.done, l.total, l.state == FileState::Here))
        })
    };
    let chip = move || {
        let (tone, text) = match state.get()? {
            FileState::Moving => return None,
            FileState::Done | FileState::Here => ("ok", "done"),
            FileState::Paused => ("warn", "paused"),
            FileState::Failed => ("err", "failed"),
            FileState::Waiting => ("off", "waiting"),
        };
        Some(view! { <span class=format!("chip {tone}")>{text}</span> })
    };
    view! {
        <li class="mf-file">
            <span class="mf-file-name">{name}</span>
            <Show when=move || state.get() == Some(FileState::Moving) fallback=move || chip>
                <span class="mf-bar" aria-hidden="true">
                    <span class="mf-bar-fill" style=move || format!("width:{}%", pct())></span>
                </span>
            </Show>
        </li>
    }
}

/// The end (§6.3): "Say hello" for a chat model, "Ready for Knowledge" for
/// an embedder or a reranker, "Ready for decisions" for a decision model.
#[component]
pub(super) fn Ready(f: Finder, done: ModelGetDone, kind: CardKind, name: String) -> impl IntoView {
    // The get made a row the gateway serves now; the session's catalog was
    // fetched before it, so the Chat "Say hello" opens would call the model
    // "not served" (WP8's live round, D110).
    if let Some(catalog) = use_context::<crate::catalog::ModelCatalog>() {
        catalog.refresh();
    }
    let ready = ReadyModel {
        id: done.model_id.clone(),
        public_name: done.public_name.clone(),
        class: done.target.clone(),
        display_name: name.clone(),
    };
    let remembers = done.note.as_ref().map(|n| match n.trained_ctx {
        Some(t) if t > n.ctx => format!(
            "It remembers {} of a conversation on this graphics card{}; the model can take {}.",
            words::tokens(n.ctx),
            match n.measure {
                FitMeasure::Beside => " beside what runs now",
                FitMeasure::WholeCard => "",
            },
            words::tokens(t),
        ),
        _ => format!("It remembers {} of a conversation.", words::tokens(n.ctx)),
    });
    let warnings = done.warnings.clone();
    let chat = done.target == "chat" || (done.target.is_empty() && kind == CardKind::Chat);
    let action = if chat {
        let r = ready.clone();
        view! {
            <button class="btn primary mf-hello" on:click=move |_| f.on_ready.run(r.clone())>
                "Say hello"
            </button>
        }
        .into_any()
    } else if done.public_name.starts_with("decide/") || kind == CardKind::Decision {
        view! {
            <span class="mf-ready-for">
                "Ready for decisions · " <a href="/decision-lab">"Open the Decision lab"</a>
            </span>
        }
        .into_any()
    } else {
        view! {
            <span class="mf-ready-for">
                "Ready for Knowledge · " <a href="/knowledge">"Make a knowledge base"</a>
            </span>
        }
        .into_any()
    };
    let lead = if done.created || done.row_id == 0 {
        format!(
            "{name} is set up as {}. It loads when it gets its first message.",
            done.public_name
        )
    } else {
        format!("{name} is already set up as {}.", done.public_name)
    };
    view! {
        <div class="mf-ready" role="status">
            <p class="mf-stage">{lead}</p>
            {remembers.map(|r| view! { <p class="mf-line">{r}</p> })}
            {(!warnings.is_empty()).then(|| view! {
                <ul class="mf-notes-list">
                    {warnings.into_iter().map(|w| view! { <li>{w}</li> }).collect_view()}
                </ul>
            })}
            <footer class="mf-actions">
                <a class="btn ghost" href="/models">"Models"</a>
                <span class="mf-spacer"></span>
                {action}
            </footer>
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_id_is_the_originals_only_for_its_uploads() {
        let card = ModelCard {
            upload: "demo-org/Demo-8B-GGUF".into(),
            other_uploads: vec![lmgw_api_types::model_find::UploadChoice {
                repo: "someone-demo/Demo-8B-GGUF".into(),
                ..Default::default()
            }],
            ..ModelCard::default()
        };
        assert!(of_the_card(&card, "demo-org/Demo-8B-GGUF"));
        assert!(of_the_card(&card, "someone-demo/demo-8b-gguf"));
        assert!(!of_the_card(&card, "elsewhere-demo/Other-GGUF"));
    }

    #[test]
    fn the_rate_is_the_downloads_own_after_a_few_seconds() {
        assert_eq!(measured_rate(None, (10_000.0, 500), Some(7.0)), Some(7.0));
        assert_eq!(
            measured_rate(Some((0.0, 0)), (1_000.0, 500), Some(7.0)),
            Some(7.0)
        );
        assert_eq!(
            measured_rate(Some((0.0, 0)), (4_000.0, 4_000), Some(7.0)),
            Some(1_000.0)
        );
        assert_eq!(measured_rate(Some((0.0, 10)), (4_000.0, 10), None), None);
    }
}
