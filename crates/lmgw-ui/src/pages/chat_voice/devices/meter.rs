//! Level meters: one `requestAnimationFrame` loop reads an `AnalyserNode`
//! per frame while something is live, and stops when nothing is (or the
//! popover is gone).

use leptos::prelude::*;

use super::super::audio::pcm::meter_level;

/// A meter's state: the level now and the highest since it was reset.
#[derive(Clone, Copy)]
pub(super) struct Meter {
    pub level: RwSignal<f64>,
    pub peak: RwSignal<f64>,
}

impl Meter {
    pub(super) fn new() -> Self {
        Self {
            level: RwSignal::new(0.0),
            peak: RwSignal::new(0.0),
        }
    }

    pub(super) fn reset(&self) {
        self.level.set(0.0);
        self.peak.set(0.0);
    }

    /// Read `analyser` into the meter.
    pub(super) fn read(&self, analyser: &web_sys::AnalyserNode, buf: &mut Vec<f32>) {
        buf.resize(analyser.fft_size() as usize, 0.0);
        analyser.get_float_time_domain_data(buf);
        let l = meter_level(buf);
        self.level.set(l);
        if l > self.peak.get_untracked() {
            self.peak.set(l);
        }
    }
}

/// Run `frame` once per animation frame while it returns `true`. `alive`
/// stops it with its owner: a frame after the popover closed reads nothing.
pub(in crate::pages::chat_voice) fn run(
    alive: crate::scope::Scope,
    frame: impl FnMut() -> bool + 'static,
) {
    fn tick(alive: crate::scope::Scope, mut frame: Box<dyn FnMut() -> bool>) {
        if !alive.alive() || !frame() {
            return;
        }
        request_animation_frame(move || tick(alive, frame));
    }
    request_animation_frame(move || tick(alive, Box::new(frame)));
}

/// The bar.
#[component]
pub(super) fn Bar(meter: Meter, #[prop(into)] label: String) -> impl IntoView {
    view! {
        <span class="vu" role="meter" aria-label=label aria-valuemin="0" aria-valuemax="1"
            aria-valuenow=move || format!("{:.2}", meter.level.get())>
            <i style=move || format!("width:{:.1}%", meter.level.get() * 100.0)></i>
        </span>
    }
}
