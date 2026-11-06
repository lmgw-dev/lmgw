//! The Voices lab's state, its calls and its wiring onto the labs' shared
//! layout: the head is the model and what it takes, the form column the
//! voice, the text and the candidates (and the cloning check), the results
//! column the kept rounds, the reference panel the voice library.

use gloo_net::http::Request;
use leptos::prelude::*;
use lmgw_api_types::voice_lab::{
    VoiceDescriptionMode, VoiceGenerateArgs, VoiceModelFacts, VoiceProvenance, VoiceRound,
    VoiceRounds, VoiceSample, VoiceSamples, VoiceSentence, VoiceSentences, DEFAULT_CANDIDATES,
};

use super::super::lab_frame::{Check, LabFrame, LabView, OwnKeep};
use super::clone_check::{CloneCard, CloneCheck};
use super::form::FormCards;
use super::head::Head;
use super::library::Library;
use super::rounds::{RoundsList, SaveState};
use crate::api;
use crate::fmt::fmt_bytes;
use crate::scope::Scope;
use crate::widgets::{use_toasts, Toasts};

/// Where the selected model's facts stand.
#[derive(Clone, Debug, Default, PartialEq)]
pub(super) enum Facts {
    #[default]
    None,
    Loading,
    Loaded(Box<VoiceModelFacts>),
    Failed(String),
}

/// `s` as a path segment or query value.
pub(super) fn enc(s: &str) -> String {
    js_sys::encode_uri_component(s).into()
}

/// The round in flight.
/// The main pane's floor beside the open voice library: the form column's
/// own (360px). The library is part of the lab's loop — save a candidate,
/// check it clones, regenerate from it — so it sits beside the rounds and
/// the rounds reflow to the room left (the columns fold to the seg below a
/// 760px body, the candidate grid to fewer columns), instead of the panel
/// opening over the candidates as the other labs' reference panels do in a
/// narrow window.
const VOICES_SIDE_FOLDS_BELOW: u32 = 360;

#[derive(Clone, Debug, PartialEq)]
pub(super) struct Pending {
    pub alias: String,
    pub count: u32,
}

/// The page's state. `Copy`: every child takes it whole.
#[derive(Clone, Copy)]
pub(super) struct Lab {
    pub alias: RwSignal<String>,
    pub facts: RwSignal<Facts>,
    /// Which facts read is the newest: an older one that lands late is
    /// dropped.
    pub facts_ticket: StoredValue<u64>,
    pub sentences: RwSignal<Vec<VoiceSentence>>,
    /// Why the owner's saved sentences are not shown (their file cannot be
    /// read); empty when they are.
    pub sentences_err: RwSignal<String>,
    /// The form's language, kept across visits.
    pub language: RwSignal<String>,
    /// Regenerate set the text with the language: the prefill leaves it.
    pub keep_text: StoredValue<bool>,
    pub text: RwSignal<String>,
    pub description: RwSignal<String>,
    pub voice: RwSignal<String>,
    /// The voice names a local model answers to, for the voice box.
    pub voices: RwSignal<Vec<String>>,
    pub count: RwSignal<String>,
    pub seed: RwSignal<String>,
    pub rounds: RwSignal<Vec<VoiceRound>>,
    pub rounds_bytes: RwSignal<u64>,
    pub rounds_dir: RwSignal<String>,
    pub running: RwSignal<bool>,
    pub pending: RwSignal<Option<Pending>>,
    pub run_err: RwSignal<String>,
    pub aborter: StoredValue<Option<web_sys::AbortController>>,
    pub samples: RwSignal<Vec<VoiceSample>>,
    pub library_active: RwSignal<bool>,
    pub samples_err: RwSignal<String>,
    /// Why no sample shows its record (the records' file cannot be read).
    pub records_err: RwSignal<String>,
    pub save: SaveState,
    pub clone: CloneCheck,
    pub view: RwSignal<LabView>,
    pub scope: Scope,
    pub toasts: Toasts,
}

impl Lab {
    /// The selected model's facts, when they are in.
    pub fn facts(self) -> Option<VoiceModelFacts> {
        match self.facts.get() {
            Facts::Loaded(f) => Some(*f),
            _ => None,
        }
    }

    /// The table's entry for `language`.
    pub fn sentence(self, language: &str) -> Option<VoiceSentence> {
        self.sentences
            .with(|v| v.iter().find(|s| s.language == language).cloned())
    }

    pub fn load_sentences(self) {
        self.scope.spawn(async move {
            match api::get::<VoiceSentences>("/voice-lab/api/sentences").await {
                Ok(t) => self.set_sentences(t),
                Err(e) => self.toasts.err(format!("sentences: {e}")),
            }
        });
    }

    pub fn set_sentences(self, t: VoiceSentences) {
        self.sentences_err.set(t.error.unwrap_or_default());
        self.sentences.set(t.languages);
    }

    pub fn load_rounds(self) {
        self.scope.spawn(async move {
            match api::get::<VoiceRounds>("/voice-lab/api/rounds").await {
                Ok(r) => self.set_rounds(r),
                Err(e) => self.toasts.err(format!("rounds: {e}")),
            }
        });
    }

    pub fn set_rounds(self, r: VoiceRounds) {
        self.rounds.set(r.rounds);
        self.rounds_bytes.set(r.bytes);
        self.rounds_dir.set(r.dir);
    }

    pub fn load_samples(self) {
        self.scope.spawn(async move {
            match api::get::<VoiceSamples>("/voice-lab/api/samples").await {
                Ok(s) => {
                    self.library_active.set(s.library_active);
                    self.samples_err.set(s.error.unwrap_or_default());
                    self.records_err.set(s.records_error.unwrap_or_default());
                    self.samples.set(s.samples);
                }
                Err(e) => self.samples_err.set(e.to_string()),
            }
        });
    }

    /// Read what the selected model takes, and its voices when it is local
    /// (a cloud model's would be a call to its provider).
    pub fn load_facts(self) {
        let alias = self.alias.get_untracked();
        let ticket = self
            .facts_ticket
            .try_update_value(|n| {
                *n += 1;
                *n
            })
            .unwrap_or_default();
        if alias.trim().is_empty() {
            self.facts.set(Facts::None);
            self.voices.set(Vec::new());
            return;
        }
        self.facts.set(Facts::Loading);
        let url = format!(
            "/voice-lab/api/model?alias={}&language={}&voice={}",
            enc(&alias),
            enc(&self.language.get_untracked()),
            enc(&self.voice.get_untracked())
        );
        self.scope.spawn(async move {
            let got = api::get::<VoiceModelFacts>(url).await;
            if self.facts_ticket.try_get_value() != Some(ticket) {
                return;
            }
            match got {
                Ok(f) => {
                    let local = f.local;
                    self.facts.set(Facts::Loaded(Box::new(f)));
                    if local {
                        self.load_voices(alias);
                    } else {
                        self.voices.set(Vec::new());
                    }
                }
                Err(e) => self.facts.set(Facts::Failed(e.to_string())),
            }
        });
    }

    fn load_voices(self, alias: String) {
        self.scope.spawn(async move {
            let url = format!("/audio-lab/api/voices?model={}", enc(&alias));
            let names = api::get::<lmgw_api_types::audio_lab::LabVoices>(url)
                .await
                .map(|v| v.voices.into_iter().map(|v| v.id).collect())
                .unwrap_or_default();
            if self.alias.get_untracked() == alias {
                self.voices.set(names);
            }
        });
    }

    /// The round's arguments as the form says them; `Err` for a count or
    /// seed that is no number.
    pub fn args(self) -> Result<VoiceGenerateArgs, String> {
        let count = parse_count(&self.count.get())?;
        let seed = parse_seed(&self.seed.get())?;
        let said = |s: String| Some(s.trim().to_string()).filter(|s| !s.is_empty());
        let f = self.facts();
        let takes_voice = f.as_ref().is_none_or(|f| f.takes_voice);
        let reads_desc = f
            .as_ref()
            .is_none_or(|f| f.description != VoiceDescriptionMode::None);
        let reads_seed = f.as_ref().is_some_and(|f| f.reads_seed);
        Ok(VoiceGenerateArgs {
            alias: self.alias.get().trim().to_string(),
            text: said(self.text.get()),
            language: said(self.language.get()),
            description: said(self.description.get()).filter(|_| reads_desc),
            voice: said(self.voice.get()).filter(|_| takes_voice),
            count: Some(count),
            seed: seed.filter(|_| reads_seed),
        })
    }

    /// The action bar's line.
    pub fn check(self) -> Check {
        if self.alias.get().trim().is_empty() {
            return Check::Needs("pick a text-to-speech model".into());
        }
        let f = match self.facts.get() {
            Facts::Loaded(f) => f,
            Facts::Loading | Facts::None => {
                return Check::Needs("reading what the model takes…".into())
            }
            Facts::Failed(e) => return Check::Invalid(e),
        };
        if self.text.get().trim().is_empty() {
            return Check::Needs("write the text the candidates say".into());
        }
        if f.description_required && self.description.get().trim().is_empty() {
            return Check::Needs(
                "describe the voice — this model designs it from the description".into(),
            );
        }
        let args = match self.args() {
            Ok(a) => a,
            Err(e) => return Check::Invalid(e),
        };
        let n = args.count.unwrap_or(DEFAULT_CANDIDATES);
        let who = match (&f.fallback, f.held) {
            (Some(fb), _) => format!(" — {fb} answers in its place now"),
            (None, true) => " — held: no fallback, it answers 503".into(),
            (None, false) => String::new(),
        };
        Check::Ready(format!(
            "{} {} from {}{who}",
            n,
            if n == 1 { "candidate" } else { "candidates" },
            args.alias
        ))
    }

    /// Fill the form from a sample's record (Regenerate): a new round of the
    /// same request, not the same audio — GPU output is not bit-identical
    /// per seed.
    pub fn regenerate(self, p: &VoiceProvenance) {
        let lang = p.language.clone().unwrap_or_default();
        if self.language.get_untracked() != lang {
            self.keep_text.set_value(true);
            self.language.set(lang);
        }
        self.text
            .set(p.asked_text.clone().unwrap_or_else(|| p.text.clone()));
        self.description
            .set(p.description.clone().unwrap_or_default());
        self.voice.set(p.voice.clone().unwrap_or_default());
        self.seed
            .set(p.seed.map(|s| s.to_string()).unwrap_or_default());
        if self.alias.get_untracked() != p.alias {
            self.alias.set(p.alias.clone());
        }
        self.view.set(LabView::Form);
        self.toasts.ok(format!(
            "the form is {}'s request again — Generate makes new candidates",
            p.alias
        ));
    }
}

/// The candidate count: a whole number, 1 or more — no ceiling.
pub(super) fn parse_count(s: &str) -> Result<u32, String> {
    let s = s.trim();
    if s.is_empty() {
        return Ok(DEFAULT_CANDIDATES);
    }
    match s.parse::<u32>() {
        Ok(n) if n >= 1 => Ok(n),
        _ => Err("candidates: a whole number, 1 or more".into()),
    }
}

/// The first seed: empty for a random one per candidate.
pub(super) fn parse_seed(s: &str) -> Result<Option<u32>, String> {
    let s = s.trim();
    if s.is_empty() {
        return Ok(None);
    }
    s.parse::<u32>()
        .map(Some)
        .map_err(|_| format!("seed: a whole number from 0 to {}", u32::MAX))
}

/// Generate: post the round, abortable.
pub(super) fn generate(lab: Lab) {
    let args = match lab.args() {
        Ok(a) => a,
        Err(e) => {
            lab.run_err.set(e);
            return;
        }
    };
    lab.run_err.set(String::new());
    lab.running.set(true);
    lab.pending.set(Some(Pending {
        alias: args.alias.clone(),
        count: args.count.unwrap_or(DEFAULT_CANDIDATES),
    }));
    lab.view.set(LabView::Results);
    lab.scope.spawn(async move {
        let ctrl = web_sys::AbortController::new().ok();
        let signal = ctrl.as_ref().map(|c| c.signal());
        lab.aborter.set_value(ctrl);
        let sent = match Request::post("/voice-lab/api/rounds")
            .abort_signal(signal.as_ref())
            .json(&args)
        {
            Ok(req) => req.send().await.map_err(|e| e.to_string()),
            Err(e) => Err(e.to_string()),
        };
        lab.aborter.set_value(None);
        match sent {
            Ok(resp) => match api::checked(resp).await {
                Ok(resp) => match resp.json::<VoiceRound>().await {
                    Ok(r) => {
                        if let Some(why) = &r.stopped {
                            lab.toasts.warn(format!("round ended early: {why}"));
                        }
                        lab.rounds.update(|v| v.insert(0, r));
                    }
                    Err(e) => lab.run_err.set(format!("bad answer: {e}")),
                },
                Err(e) => lab.run_err.set(e.to_string()),
            },
            Err(e) if e.to_lowercase().contains("abort") => {}
            Err(e) => lab.run_err.set(format!("not sent: {e}")),
        }
        lab.pending.set(None);
        lab.running.set(false);
        // A stopped round keeps what it made; the list says what is kept.
        lab.load_rounds();
    });
}

/// Stop: abort the round in flight (the server keeps the candidates made).
pub(super) fn stop(lab: Lab) {
    if let Some(a) = lab.aborter.get_value() {
        a.abort();
    }
}

#[component]
pub fn VoiceLab() -> impl IntoView {
    let scope = Scope::new();
    let lab = Lab {
        // The model picked last, kept across visits like the language.
        alias: crate::prefs::persisted_string("voicelab.alias", ""),
        facts: RwSignal::new(Facts::None),
        facts_ticket: StoredValue::new(0),
        sentences: RwSignal::new(Vec::new()),
        sentences_err: RwSignal::new(String::new()),
        language: crate::prefs::persisted_string("voicelab.language", "en"),
        keep_text: StoredValue::new(false),
        text: RwSignal::new(String::new()),
        description: RwSignal::new(String::new()),
        voice: RwSignal::new(String::new()),
        voices: RwSignal::new(Vec::new()),
        count: RwSignal::new(DEFAULT_CANDIDATES.to_string()),
        seed: RwSignal::new(String::new()),
        rounds: RwSignal::new(Vec::new()),
        rounds_bytes: RwSignal::new(0),
        rounds_dir: RwSignal::new(String::new()),
        running: RwSignal::new(false),
        pending: RwSignal::new(None),
        run_err: RwSignal::new(String::new()),
        aborter: StoredValue::new(None),
        samples: RwSignal::new(Vec::new()),
        library_active: RwSignal::new(true),
        samples_err: RwSignal::new(String::new()),
        records_err: RwSignal::new(String::new()),
        save: SaveState::new(),
        clone: CloneCheck::new(),
        view: RwSignal::new(LabView::Form),
        scope,
        toasts: use_toasts(),
    };
    lab.load_sentences();
    lab.load_rounds();
    lab.load_samples();
    // The model's facts, read again when it or the language changes (the
    // language note depends on both).
    Effect::new(move |_| {
        lab.alias.track();
        lab.language.track();
        lab.load_facts();
    });
    // The language's sentence is the text's prefill, once the table is in
    // and whenever the language changes — unless Regenerate set both.
    Effect::new(move |prev: Option<Option<String>>| {
        let lang = lab.language.get();
        if lab.sentences.with(Vec::is_empty) {
            return prev.flatten();
        }
        if prev.flatten().as_deref() != Some(lang.as_str()) {
            if lab.keep_text.get_value() {
                lab.keep_text.set_value(false);
            } else {
                let text = lab.sentence(&lang).map(|s| s.text).unwrap_or_default();
                lab.text.set(text);
            }
        }
        Some(lang)
    });
    on_cleanup(move || {
        lab.aborter
            .try_with_value(|a| a.as_ref().map(web_sys::AbortController::abort));
    });

    let check = Memo::new(move |_| lab.check());
    let own_keep = OwnKeep {
        note: Signal::derive(move || {
            format!(
                "newest first · kept until discarded · {}",
                fmt_bytes(lab.rounds_bytes.get())
            )
        }),
        title: "Rounds are kept on the server (the data dir's voice-lab/rounds) until \
                discarded; a saved sample is a copy in the voice library and stays",
        empty: "Every round lands here, newest first, and is kept on the server until you \
                discard it — one from its card, or all of them with Clear. Save the candidate \
                you like: it goes to the voice library with its transcript.",
        clear_confirm: Signal::derive(move || {
            let n = lab.rounds.with(Vec::len);
            format!(
                "Discard all {n} {} ({})? Saved samples stay in the library.",
                if n == 1 { "round" } else { "rounds" },
                fmt_bytes(lab.rounds_bytes.get())
            )
        }),
        clear_title: "Discard every kept round and its candidates' audio",
    };
    let clear = Callback::new(move |()| {
        lab.scope.spawn(async move {
            match api::post::<VoiceRounds, _>("/voice-lab/api/rounds/clear", &()).await {
                Ok(r) => {
                    lab.set_rounds(r);
                    lab.toasts.ok("every round discarded");
                }
                Err(e) => lab.toasts.err(e.to_string()),
            }
        });
    });

    view! {
        <LabFrame
            persist="voice-lab.library"
            side_label="Voice library"
            side_badge=Signal::derive(move || {
                let n = lab.samples.with(Vec::len);
                if n == 0 { String::new() } else { n.to_string() }
            })
            side=move || view! { <Library lab=lab/> }
            head=move || view! { <Head lab=lab/> }
            form=move || {
                view! {
                    <FormCards lab=lab/>
                    <CloneCard lab=lab/>
                }
            }
            results=move || view! { <RoundsList lab=lab/> }
            results_n=Signal::derive(move || lab.rounds.with(Vec::len))
            check=check
            run_label=Signal::derive(|| "Generate".to_string())
            running=lab.running
            on_run=Callback::new(move |()| generate(lab))
            on_stop=Callback::new(move |()| stop(lab))
            on_clear=clear
            view=lab.view
            own_keep=own_keep
            side_folds_below=VOICES_SIDE_FOLDS_BELOW
        />
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_count_has_a_floor_and_no_ceiling() {
        assert_eq!(parse_count(""), Ok(DEFAULT_CANDIDATES));
        assert_eq!(parse_count(" 12 "), Ok(12));
        assert_eq!(parse_count("5000"), Ok(5000));
        assert!(parse_count("0").is_err());
        assert!(parse_count("-1").is_err());
        assert!(parse_count("two").is_err());
    }

    #[test]
    fn a_blank_seed_is_a_random_one() {
        assert_eq!(parse_seed(""), Ok(None));
        assert_eq!(parse_seed("42"), Ok(Some(42)));
        assert!(parse_seed("4294967296").is_err());
    }
}
