//! One download for the graphics card (lmgw's machine-check design §5,
//! D14): a model's engine image that is not on this machine yet, said the
//! same wherever it shows — the dashboard's GPU popover, Overview, Models
//! list and labs, and the Chat's tool card and turn line (`lmgw-chat-ui`).
//! The live sources (the `vram` and jobs frames) stay with each host; this
//! is the state, its words and its bar.
//!
//! The sentence is "Download for your graphics card · 1.2 of 3.4 GiB" (a
//! chip says "graphics card download · 1.2 of 3.4 GiB"), with a bar and a
//! tooltip that has the image reference and, when the size is not known,
//! why. The plain sentence never says "engine", "container" or "image";
//! the reference is the detail.
//!
//! The bytes are as Podman reports them: about one update a second per layer
//! in flight, none for a layer that lands within a second, and a total that
//! may not be known (no registry manifest, or the `podman pull` command's
//! path, which counts no bytes). So the bar is determinate only with a
//! total; without one it is an indeterminate bar, never a made-up fraction.
//! Once every byte is in, Podman still unpacks the layers, which it does not
//! count: the line says "unpacking" then, and the bar is indeterminate again
//! rather than standing full.

use leptos::prelude::*;
use lmgw_api_types::builds::ImagePullJobDetail;
use lmgw_api_types::chat_media::ToolProgressFrame;
use lmgw_api_types::chat_voice::ModelState;
// The sizes in lmgw's binary units (D160): the unit by the rounded value,
// the part done rounded down and never said as the total while bytes miss.
use lmgw_api_types::units::{fmt_bytes as size, fmt_bytes_of as of, fmt_bytes_so_far as so_far};
use lmgw_api_types::{JobRow, VramWaiter, WaitPhase};

/// The plain sentence (D14).
pub const SENTENCE: &str = "Download for your graphics card";

/// The same, as a state chip's label says it, where it stands beside
/// "ready" and "stopped" (D39).
pub const CHIP: &str = "graphics card download";

/// The kind of the jobs that pull an image.
pub const PULL_KIND: &str = "image_pull";

/// One download's state, from whichever frame said it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Dl {
    /// The image reference: the detail, never the sentence.
    pub image: String,
    pub done: u64,
    /// `None` while the size is not known.
    pub total: Option<u64>,
    /// Why the size is not known, when something said.
    pub note: Option<String>,
}

impl Dl {
    pub fn of_phase(p: &WaitPhase) -> Option<Self> {
        match p {
            WaitPhase::Downloading {
                image,
                done_bytes,
                total_bytes,
                size_note,
            } => Some(Self {
                image: image.clone(),
                done: *done_bytes,
                total: *total_bytes,
                note: size_note.clone(),
            }),
            _ => None,
        }
    }

    pub fn of_waiter(w: &VramWaiter) -> Option<Self> {
        w.phase.as_ref().and_then(Self::of_phase)
    }

    /// An `image_pull` job on the jobs feed: its bytes are the job's own
    /// `done`/`total`, its note in the detail.
    pub fn of_job(j: &JobRow) -> Option<Self> {
        if j.kind != PULL_KIND {
            return None;
        }
        let detail: ImagePullJobDetail =
            serde_json::from_value(j.detail.clone()).unwrap_or_default();
        let image = j
            .key
            .as_deref()
            .and_then(|k| k.strip_prefix("image:"))
            .map(str::to_string)
            .or_else(|| j.label.strip_prefix("pull ").map(str::to_string))
            .unwrap_or_default();
        Some(Self {
            image,
            done: j.done.max(detail.done_bytes),
            total: j.total.or(detail.total_bytes),
            note: detail.size_note,
        })
    }

    /// An `image_pull` job's detail (the Engines page's live pulls).
    pub fn of_detail(image: &str, d: &ImagePullJobDetail) -> Self {
        Self {
            image: image.to_string(),
            done: d.done_bytes,
            total: d.total_bytes,
            note: d.size_note.clone(),
        }
    }

    /// A media call's `downloading` progress frame.
    pub fn of_frame(f: &ToolProgressFrame) -> Option<Self> {
        (f.phase == Some(lmgw_api_types::chat_media::MediaPhase::Downloading)).then(|| Self {
            image: f.image.clone().unwrap_or_default(),
            done: f.done_bytes.unwrap_or(0),
            total: f.total_bytes,
            note: f.size_note.clone(),
        })
    }

    /// A chat turn's `downloading` state frame.
    pub fn of_state(s: &ModelState) -> Option<Self> {
        (s.state == "downloading").then(|| Self {
            image: s.image.clone().unwrap_or_default(),
            done: s.done_bytes.unwrap_or(0),
            total: s.total_bytes,
            note: s.size_note.clone(),
        })
    }

    /// The total, when one is known and is more than nothing.
    fn known_total(&self) -> Option<u64> {
        self.total.filter(|t| *t > 0)
    }

    /// Every byte is in while the download still runs: Podman unpacks the
    /// layers, which it does not count (module doc).
    pub fn unpacking(&self) -> bool {
        self.known_total().is_some_and(|t| self.done >= t)
    }

    /// The bytes in words: "1.2 of 3.4 GiB", "120 of 308 MiB", "1.2 GiB so
    /// far" (size not known), "unpacking 3.4 GiB" once every byte is in, or
    /// nothing before the first byte of a size not known.
    pub fn amount(&self) -> Option<String> {
        match self.known_total() {
            Some(t) if self.done >= t => Some(format!("unpacking {}", size(t))),
            Some(t) => Some(of(self.done, t)),
            None if self.done > 0 => Some(format!("{} so far", so_far(self.done))),
            None => None,
        }
    }

    /// The whole line: "Download for your graphics card · 1.2 of 3.4 GiB".
    pub fn words(&self) -> String {
        match self.amount() {
            Some(a) => format!("{SENTENCE} · {a}"),
            None => SENTENCE.to_string(),
        }
    }

    /// The chip's label: "graphics card download · 1.2 of 3.4 GiB".
    pub fn chip(&self) -> String {
        match self.amount() {
            Some(a) => format!("{CHIP} · {a}"),
            None => CHIP.to_string(),
        }
    }

    /// The tooltip: the reference, and why the size is not known.
    pub fn title(&self) -> String {
        let mut t = self.image.clone();
        match (&self.total, &self.note) {
            (None, Some(n)) => {
                t.push('\n');
                t.push_str(n);
            }
            (None, None) => t.push_str("\nsize not known yet"),
            _ => {}
        }
        t
    }

    /// How far, 0–100; `None` while the total is not known.
    pub fn percent(&self) -> Option<f64> {
        self.total
            .filter(|t| *t > 0)
            .map(|t| (self.done.min(t) as f64 / t as f64 * 100.0).clamp(0.0, 100.0))
    }

    /// The share the bar draws: none (an indeterminate bar) while the size
    /// is not known or the layers unpack.
    fn bar(&self) -> Option<f64> {
        self.percent().filter(|_| !self.unpacking())
    }
}

/// The bar: filled to the share done with a total, indeterminate without
/// one and while the layers unpack.
#[component]
pub fn DlBar(#[prop(into)] dl: Signal<Dl>) -> impl IntoView {
    let pct = move || dl.with(Dl::bar);
    view! {
        <div
            class="progress dl-bar"
            class:indeterminate=move || pct().is_none()
            role="progressbar"
            aria-valuemin="0"
            aria-valuemax="100"
            aria-valuenow=move || pct().map(|p| format!("{p:.0}"))
            aria-label=SENTENCE
        >
            <i style=move || match pct() {
                Some(p) => format!("width:{p:.1}%"),
                None => String::new(),
            }></i>
        </div>
    }
}

/// The line: the sentence with its bytes, `who` beside it (who it is for,
/// where the surface does not say so already), and the bar under them.
/// The tooltip has the reference and the size note.
#[component]
pub fn DlLine(
    #[prop(into)] dl: Signal<Dl>,
    #[prop(optional, into)] who: MaybeProp<String>,
) -> impl IntoView {
    view! {
        <div class="dl-gpu" title=move || dl.with(Dl::title) data-gpu-download="">
            <span class="dl-gpu-words">{move || dl.with(Dl::words)}</span>
            {move || who.get().map(|w| view! { <span class="dim dl-gpu-who">{w}</span> })}
            <DlBar dl=dl/>
        </div>
    }
}

/// A state chip for a model whose start waits for its download:
/// "graphics card download · 1.2 of 3.4 GiB", the sentence and the reference
/// in its tooltip.
#[component]
pub fn DlChip(#[prop(into)] dl: Signal<Dl>) -> impl IntoView {
    let title = move || dl.with(|d| format!("{}\n{}", d.words(), d.title()));
    view! {
        <span class="chip live" title=title data-gpu-download="">
            <span class="dot"></span>
            {move || dl.with(Dl::chip)}
        </span>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dl(done: u64, total: Option<u64>, note: Option<&str>) -> Dl {
        Dl {
            image: "ghcr.io/ggml-org/llama.cpp:server-cuda".into(),
            done,
            total,
            note: note.map(str::to_string),
        }
    }

    const MIB: u64 = 1 << 20;
    const GIB: u64 = 1 << 30;

    #[test]
    fn the_line_says_the_bytes_in_the_totals_unit() {
        assert_eq!(
            dl(GIB * 12 / 10, Some(GIB * 34 / 10), None).words(),
            "Download for your graphics card · 1.1 of 3.4 GiB"
        );
        assert_eq!(
            dl(120 * MIB, Some(308 * MIB), None).words(),
            "Download for your graphics card · 120 of 308 MiB"
        );
        assert_eq!(
            dl(0, Some(GIB * 34 / 10), None).words(),
            "Download for your graphics card · 0.0 of 3.4 GiB"
        );
        // A done past the total (a layer Podman counted twice) is the total.
        assert_eq!(dl(5, Some(4), None).percent(), Some(100.0));
    }

    #[test]
    fn the_chip_says_whose_download_it_is() {
        assert_eq!(
            dl(GIB * 12 / 10, Some(GIB * 34 / 10), None).chip(),
            "graphics card download · 1.1 of 3.4 GiB"
        );
        assert_eq!(dl(0, None, None).chip(), "graphics card download");
    }

    /// Review L4: the unit by the rounded value, the done rounded down and
    /// never the total while unfinished.
    #[test]
    fn the_figures_never_mislead() {
        // A total that rounds to 1024 MiB is a GiB.
        assert_eq!(of(400 * MIB, 1024 * MIB - 30_000), "0.3 of 1.0 GiB");
        assert_eq!(size(1024 * MIB - 30_000), "1.0 GiB");
        assert_eq!(size(1000 * MIB), "1000 MiB");
        // About 1% left: not yet the total.
        let t = GIB * 34 / 10;
        assert_eq!(of(t - t / 100, t), "3.3 of 3.4 GiB");
        assert_eq!(of(t - 1, t), "3.3 of 3.4 GiB");
        assert_eq!(of(307 * MIB + MIB / 2, 308 * MIB), "307 of 308 MiB");
        // Rounded down, never up.
        assert_eq!(of(GIB * 129 / 100, t), "1.2 of 3.4 GiB");
        // Tiny totals in bytes, and nothing known.
        assert_eq!(of(100, 300), "100 of 300 B");
        assert_eq!(dl(0, Some(0), None).amount(), None, "a total of 0 is none");
        assert_eq!(dl(0, Some(0), None).percent(), None);
        assert_eq!(
            dl(300_000, None, None).words(),
            "Download for your graphics card · 292 KiB so far"
        );
        assert_eq!(
            dl(GIB * 129 / 100, None, None).amount().as_deref(),
            Some("1.2 GiB so far")
        );
        assert_eq!(
            dl(1024 * MIB - 30_000, None, None).amount().as_deref(),
            Some("1023 MiB so far"),
            "the unit follows the rounded-down figure, never 0.9 GiB"
        );
        // Bytes in against a total of 0 are a size not known, not "unpacking 0 B".
        let zero = dl(300_000, Some(0), None);
        assert!(!zero.unpacking());
        assert_eq!(zero.amount().as_deref(), Some("292 KiB so far"));
    }

    /// Review M1: every byte in while the job runs is the unpack, said so,
    /// and the bar no longer stands full.
    #[test]
    fn every_byte_in_is_unpacking_not_a_full_bar() {
        let d = dl(GIB * 34 / 10, Some(GIB * 34 / 10), None);
        assert!(d.unpacking());
        assert_eq!(
            d.words(),
            "Download for your graphics card · unpacking 3.4 GiB"
        );
        assert_eq!(d.chip(), "graphics card download · unpacking 3.4 GiB");
        assert_eq!(d.bar(), None, "indeterminate");
        assert_eq!(dl(5, Some(4), None).bar(), None);
        let moving = dl(GIB * 12 / 10, Some(GIB * 34 / 10), None);
        assert!(!moving.unpacking());
        assert!(moving.bar().is_some());
    }

    #[test]
    fn a_size_not_known_says_so_far_and_why_and_draws_no_fraction() {
        let d = dl(
            GIB * 12 / 10,
            None,
            Some("size not known — the API was not reachable"),
        );
        assert_eq!(
            d.words(),
            "Download for your graphics card · 1.1 GiB so far"
        );
        assert_eq!(d.percent(), None);
        assert!(d
            .title()
            .ends_with("size not known — the API was not reachable"));
        let fresh = dl(0, None, None);
        assert_eq!(fresh.words(), SENTENCE);
        assert!(fresh.title().contains("size not known yet"));
        for w in [d.words(), fresh.words(), d.chip()] {
            for word in ["engine", "container", "image"] {
                assert!(!w.contains(word), "{word} in {w}");
            }
        }
    }
}
