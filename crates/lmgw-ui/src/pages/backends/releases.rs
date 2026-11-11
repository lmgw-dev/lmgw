//! The Windows tab (Windows design §4.2): each engine's pinned upstream
//! release build — the one this machine runs and why, what is installed,
//! and whether upstream has published a newer Windows release than lmgw's
//! pin (from the same check as the registry images, **Check now** in the
//! page's head). Shown where lmgw starts its engines as native processes;
//! on Linux the engines are the Images tab's.
//!
//! The builds are installed by a model's first start (download, digest
//! check, unpack); the tab has no install button yet. A CUDA build marked
//! "no kernel image" shows the engine's line and how its process ended,
//! and **Try CUDA again** clears the mark (`engine_fallback_clear`). Where
//! the engines run as containers (a hand-typed `/backends/windows` on
//! Linux) the tab says so instead of a table.

use leptos::prelude::*;
use lmgw_api_types::engine_releases::{
    EngineBuild, EngineRelease, NoKernelImage, ReleaseCheck, SupportRelease,
};

use super::{ago, local_ts, use_bk};
use crate::fmt::fmt_bytes;

const COLS: u32 = 5;

/// The release check as a chip: `(class, label, tooltip)`.
pub fn release_chip(pinned: &str, r: Option<&ReleaseCheck>) -> (&'static str, String, String) {
    let Some(r) = r else {
        return (
            "chip off",
            "not checked".into(),
            "No release check has run yet: Check now asks GitHub.".into(),
        );
    };
    let when = if r.checked_at.is_empty() {
        String::new()
    } else {
        format!(
            "\nchecked {} ({})",
            ago(&r.checked_at),
            local_ts(&r.checked_at)
        )
    };
    if let Some(e) = &r.error {
        return ("chip off", "not checked".into(), format!("{e}{when}"));
    }
    let latest = r.latest_tag.clone().unwrap_or_default();
    if r.update_available {
        (
            "chip warn",
            format!("{latest} upstream"),
            format!(
                "Upstream published {latest}{}, newer than lmgw's pin {pinned}. lmgw runs its \
                 pinned builds; the newer one comes with the lmgw release that pins it.{when}",
                r.latest_published
                    .as_deref()
                    .map(|p| format!(" ({})", local_ts(p)))
                    .unwrap_or_default()
            ),
        )
    } else {
        (
            "chip ok",
            "newest".into(),
            format!("{pinned} is upstream's newest Windows release.{when}"),
        )
    }
}

/// One build's state in words: installed, marked by the fallback, or not
/// downloaded yet.
pub fn build_state(b: &EngineBuild) -> (&'static str, String) {
    if b.no_kernel_image.is_some() {
        return ("chip warn", "no kernel image here".into());
    }
    if b.installed {
        ("chip ok", "installed".into())
    } else {
        (
            "chip off",
            format!("not downloaded ({})", fmt_bytes(b.download_bytes)),
        )
    }
}

/// A mark's reason, for its tooltip: the line, how the process ended, when.
pub fn mark_reason(m: &NoKernelImage) -> String {
    let mut t = m.line.clone();
    if !m.exit.is_empty() {
        t.push_str(&format!("\nThe process {}.", m.exit));
    }
    if !m.marked_at.is_empty() {
        t.push_str(&format!(
            "\nMarked {} ({}).",
            ago(&m.marked_at),
            local_ts(&m.marked_at)
        ));
    }
    t.push_str("\nThe engine runs its Vulkan build until the mark is cleared.");
    t
}

#[component]
pub fn ReleasesTab() -> impl IntoView {
    let bk = use_bk();
    let native = move || bk.releases.with(|r| r.as_ref().is_none_or(|r| r.native));
    view! {
        <Show when=move || !native()>
            <div class="notice row">
                "lmgw runs its engines as containers here (the Images tab); the pinned Windows \
                 release builds are not used on this platform."
            </div>
        </Show>
        <Show when=native>
            <ReleasesTable/>
        </Show>
    }
}

#[component]
fn ReleasesTable() -> impl IntoView {
    let bk = use_bk();
    view! {
        {move || {
            bk.releases_err
                .get()
                .map(|e| {
                    view! {
                        <div class="notice err row">
                            "Reading the engine builds failed: "
                            {e}
                            <button class="btn ghost sm" on:click=move |_| bk.load_releases()>
                                "Retry"
                            </button>
                        </div>
                    }
                })
        }}
        {move || {
            bk.releases
                .get()
                .map(|r| {
                    let m = r.machine.clone();
                    let cards = if m.cards.is_empty() {
                        match &m.nvml_error {
                            Some(e) => format!("no NVIDIA card ({e})"),
                            None => "no NVIDIA card".to_string(),
                        }
                    } else {
                        m.cards.join(", ")
                    };
                    let driver = m
                        .driver_cuda
                        .map(|d| format!(" · driver CUDA {d}"))
                        .unwrap_or_default();
                    let vulkan = if m.vulkan_loader {
                        " · Vulkan loader installed"
                    } else {
                        " · no Vulkan loader"
                    };
                    view! {
                        <div class="bk-foot">
                            <span>"This machine: " <b>{cards}</b> {driver} {vulkan}</span>
                            <span class="dim">"Builds in " {r.engines_dir.clone()}</span>
                        </div>
                    }
                })
        }}
        <div class="fill-pane card pad0">
            <table class="data bk-releases">
                <thead>
                    <tr>
                        <th>"Engine"</th>
                        <th>"Pinned release"</th>
                        <th title="The build this machine runs: CUDA 13, CUDA 12 where its recorded coverage reaches every card, Vulkan, CPU">
                            "Runs here"
                        </th>
                        <th>"Builds"</th>
                        <th class="col-p3">"Upstream"</th>
                    </tr>
                </thead>
                <tbody>
                    <Show when=move || bk.releases.with(Option::is_none) && bk.releases_err.with(Option::is_none)>
                        <tr>
                            <td colspan=COLS class="dim">"Loading…"</td>
                        </tr>
                    </Show>
                    {move || {
                        bk.releases
                            .get()
                            .map(|r| {
                                let rows: Vec<_> = r.engines.iter().map(engine_row).collect();
                                let espeak = r.espeak.as_ref().map(espeak_row);
                                view! { {rows} {espeak} }
                            })
                    }}
                </tbody>
            </table>
        </div>
    }
}

fn engine_row(e: &EngineRelease) -> impl IntoView {
    let bk = use_bk();
    let (chip, label, tip) = release_chip(&e.tag, e.release.as_ref());
    let builds: Vec<_> = e
        .builds
        .iter()
        .map(|b| {
            let (c, state) = build_state(b);
            let mut title = format!("{}\n{}", b.assets.join(" + "), b.dir);
            if let Some(t) = &b.cuda_toolkit {
                title.push_str(&format!("\nCUDA {t}"));
            }
            for line in &b.cuda_coverage {
                title.push('\n');
                title.push_str(line);
            }
            let chosen = b.variant == e.chosen;
            let mark = b.no_kernel_image.as_ref().map(|m| {
                let (engine, variant) = (e.engine.clone(), b.variant.clone());
                view! {
                    <div class="dim" title=mark_reason(m)>
                        {m.line.clone()}
                        {(!m.exit.is_empty()).then(|| format!(" (the process {})", m.exit))}
                        " "
                        <button
                            class="btn ghost sm"
                            title="Clear the mark: the next start of this engine chooses its build from the machine's facts again, which may be this CUDA build"
                            on:click=move |_| bk.clear_fallback(engine.clone(), variant.clone())
                        >
                            "Try CUDA again"
                        </button>
                    </div>
                }
            });
            view! {
                <div class="bk-release-build" title=title>
                    <span class=if chosen { "type-badge" } else { "dim" }>{b.variant.clone()}</span>
                    " "
                    <span class=c>{state}</span>
                    {mark}
                </div>
            }
        })
        .collect();
    let jit = e.chosen_jit.then(|| {
        view! {
            <div class="dim">"its first start compiles the kernels (minutes on an older card)"</div>
        }
    });
    view! {
        <tr>
            <td>
                <b>{e.label.clone()}</b>
                <div class="dim">{e.repo.clone()}</div>
            </td>
            <td>
                {e.tag.clone()}
                <div class="dim">{e.published.clone()}</div>
            </td>
            <td title=e.chosen_why.clone()>
                <span class="type-badge">{e.chosen.clone()}</span>
                {jit}
            </td>
            <td>{builds}</td>
            <td>
                <span class=chip title=tip>{label}</span>
            </td>
        </tr>
    }
}

fn espeak_row(s: &SupportRelease) -> impl IntoView {
    let (chip, label, tip) = release_chip(&s.tag, s.release.as_ref());
    let state = if s.installed {
        ("chip ok", "installed".to_string())
    } else {
        (
            "chip off",
            format!("not downloaded ({})", fmt_bytes(s.download_bytes)),
        )
    };
    view! {
        <tr>
            <td>
                <b>{s.label.clone()}</b>
                <div class="dim">{s.repo.clone()} " · for audio.cpp's phonemizing voices"</div>
            </td>
            <td>
                {s.tag.clone()}
                <div class="dim">{s.published.clone()}</div>
            </td>
            <td class="dim">"with audio.cpp"</td>
            <td title=format!("{}\n{}", s.asset, s.dir)>
                <span class=state.0>{state.1}</span>
            </td>
            <td>
                <span class=chip title=tip>{label}</span>
            </td>
        </tr>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_release_chip_says_update_newest_or_not_checked() {
        assert_eq!(release_chip("b1", None).1, "not checked");
        let r = ReleaseCheck {
            latest_tag: Some("b2".into()),
            update_available: true,
            ..Default::default()
        };
        let (c, l, t) = release_chip("b1", Some(&r));
        assert_eq!((c, l.as_str()), ("chip warn", "b2 upstream"));
        assert!(t.contains("newer than lmgw's pin b1"), "{t}");
        let r = ReleaseCheck {
            latest_tag: Some("b1".into()),
            ..Default::default()
        };
        assert_eq!(release_chip("b1", Some(&r)).1, "newest");
        let r = ReleaseCheck {
            error: Some("check failed: 403".into()),
            ..Default::default()
        };
        assert_eq!(release_chip("b1", Some(&r)).0, "chip off");
    }

    #[test]
    fn a_build_says_installed_marked_or_its_download_size() {
        let mut b = EngineBuild {
            download_bytes: 300 * 1024 * 1024,
            ..Default::default()
        };
        assert_eq!(build_state(&b).1, "not downloaded (300 MiB)");
        b.installed = true;
        assert_eq!(build_state(&b).1, "installed");
        b.no_kernel_image = Some(NoKernelImage {
            line: "CUDA error: no kernel image is available".into(),
            exit: "exited with code 1".into(),
            marked_at: String::new(),
        });
        assert!(build_state(&b).1.starts_with("no kernel image here"));
        let why = mark_reason(b.no_kernel_image.as_ref().unwrap());
        assert!(why.starts_with("CUDA error: no kernel image"), "{why}");
        assert!(why.contains("The process exited with code 1."), "{why}");
    }
}
