//! The form as the page edits it (labs-store design §2.3): the state, the
//! images and the questions, each question, option and level with a page-own
//! `uid`, so a keyed list moves a row's inputs with it and leaves them alone
//! while they are typed in. [`FormState::draft`] is the wire's
//! [`DecisionForm`], which the count and Ask send; what follows each
//! keystroke (the action bar's check, the gauge's trigger, the preview,
//! dirty) reads [`FormState::text_draft`] and the images in place, so an
//! image's data URL is not copied per keystroke.
//!
//! A question keeps every type's fields: switching it from `choice` to
//! `score` and back gives its options back.

use leptos::prelude::*;
use lmgw_api_types::decision_lab::form::{elide_data_url, human_bytes};
use lmgw_api_types::decision_lab::{
    DecisionForm, ImageInput, OptionDraft, QuestionDraft, QuestionKind, StateMode,
};
use lmgw_api_types::lab_store::{artifact_ref, LabArtifactView};

/// A question's type, as the type select names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Kind {
    Choice,
    Score,
    Noul,
}

impl Kind {
    pub const ALL: [Self; 3] = [Self::Choice, Self::Score, Self::Noul];

    pub fn wire(self) -> &'static str {
        match self {
            Self::Choice => "choice",
            Self::Score => "score",
            Self::Noul => "noul",
        }
    }

    pub fn parse(s: &str) -> Self {
        Self::ALL
            .into_iter()
            .find(|k| k.wire() == s)
            .unwrap_or(Self::Choice)
    }

    /// The select's words: the wire's name, and what it answers.
    pub fn label(self) -> &'static str {
        match self {
            Self::Choice => "choice · one of",
            Self::Score => "score · levels",
            Self::Noul => "noul · yes/no",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Opt {
    pub uid: u32,
    pub name: String,
    pub description: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Level {
    pub uid: u32,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Q {
    pub uid: u32,
    pub id: String,
    pub instructions: String,
    pub kind: Kind,
    pub options: Vec<Opt>,
    /// Lowest first.
    pub levels: Vec<Level>,
    pub yes: String,
    pub no: String,
}

impl Q {
    pub fn draft(&self) -> QuestionDraft {
        let kind = match self.kind {
            Kind::Choice => QuestionKind::Choice {
                options: self
                    .options
                    .iter()
                    .map(|o| OptionDraft {
                        name: o.name.clone(),
                        description: o.description.clone(),
                    })
                    .collect(),
            },
            Kind::Score => QuestionKind::Score {
                levels: self.levels.iter().map(|l| l.text.clone()).collect(),
            },
            Kind::Noul => QuestionKind::Noul {
                yes: self.yes.clone(),
                no: self.no.clone(),
            },
        };
        QuestionDraft {
            id: self.id.clone(),
            instructions: self.instructions.clone(),
            kind,
        }
    }
}

/// One image the form sends.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Img {
    pub uid: u32,
    pub input: ImageInput,
    /// Its size in bytes, for the thumbnail's caption.
    pub size: u64,
    /// What an `<img>` shows: the data URL, or the stored file's href.
    pub src: String,
    /// A kept image's data URL head as it was typed, which its request
    /// sends it with.
    pub head: Option<String>,
}

impl Img {
    /// The image as the preview names it: a data URL's payload elided
    /// ("data:image/png;base64,… 1.4 MB"), a kept image the same way from
    /// its head and size, or by its ref when it has no head.
    pub fn shown(&self) -> String {
        match (&self.input, &self.head) {
            (ImageInput::DataUrl { url, .. }, _) => elide_data_url(url),
            (ImageInput::Artifact { .. }, Some(head)) => {
                format!("{head},… {}", human_bytes(self.size))
            }
            (ImageInput::Artifact { id, .. }, None) => artifact_ref(*id),
        }
    }

    /// The image as a body sends it: a data URL whole, a kept image by its
    /// `lab-artifact:<id>`, which the server sends as its data URL.
    pub fn sent(&self) -> String {
        match &self.input {
            ImageInput::DataUrl { url, .. } => url.clone(),
            ImageInput::Artifact { id, .. } => artifact_ref(*id),
        }
    }
}

/// Hands out the page's row ids.
#[derive(Clone, Copy)]
pub(super) struct Uids(StoredValue<u32>);

impl Uids {
    pub fn new() -> Self {
        Self(StoredValue::new(1))
    }

    pub fn next(self) -> u32 {
        self.0
            .try_update_value(|n| {
                *n += 1;
                *n
            })
            .unwrap_or_default()
    }
}

/// `QuestionDraft`s as rows, numbered by `next`.
pub(super) fn rows_of(drafts: Vec<QuestionDraft>, mut next: impl FnMut() -> u32) -> Vec<Q> {
    drafts
        .into_iter()
        .map(|d| {
            let mut q = Q {
                uid: next(),
                id: d.id,
                instructions: d.instructions,
                kind: Kind::Choice,
                options: Vec::new(),
                levels: Vec::new(),
                yes: String::new(),
                no: String::new(),
            };
            match d.kind {
                QuestionKind::Choice { options } => {
                    q.options = options
                        .into_iter()
                        .map(|o| Opt {
                            uid: next(),
                            name: o.name,
                            description: o.description,
                        })
                        .collect();
                }
                QuestionKind::Score { levels } => {
                    q.kind = Kind::Score;
                    q.levels = levels
                        .into_iter()
                        .map(|text| Level { uid: next(), text })
                        .collect();
                }
                QuestionKind::Noul { yes, no } => {
                    q.kind = Kind::Noul;
                    q.yes = yes;
                    q.no = no;
                }
            }
            q
        })
        .collect()
}

/// A fresh question: the next free `q<n>` id, a choice with two empty
/// options.
pub(super) fn new_question(taken: &[Q], mut next: impl FnMut() -> u32) -> Q {
    let id = (1..)
        .map(|n| format!("q{n}"))
        .find(|c| !taken.iter().any(|q| &q.id == c))
        .unwrap_or_default();
    Q {
        uid: next(),
        id,
        instructions: String::new(),
        kind: Kind::Choice,
        options: vec![
            Opt {
                uid: next(),
                name: String::new(),
                description: String::new(),
            },
            Opt {
                uid: next(),
                name: String::new(),
                description: String::new(),
            },
        ],
        levels: Vec::new(),
        yes: String::new(),
        no: String::new(),
    }
}

/// Move the item at `from` one place up (`-1`) or down (`1`).
pub(super) fn shift<T>(v: &mut [T], from: usize, by: isize) {
    let to = from as isize + by;
    if to >= 0 && (to as usize) < v.len() {
        v.swap(from, to as usize);
    }
}

/// The editable form. `Copy`.
#[derive(Clone, Copy)]
pub(super) struct FormState {
    pub mode: RwSignal<StateMode>,
    pub state: RwSignal<String>,
    pub images: RwSignal<Vec<Img>>,
    pub questions: RwSignal<Vec<Q>>,
    pub uids: Uids,
    /// The form as it was last loaded: an example, an opened group, the
    /// empty form. The form is dirty when it differs.
    pub loaded: RwSignal<DecisionForm>,
    dirty: Memo<bool>,
}

/// Is the form as it is (`text`, its questions with the images left out,
/// and `images`) the one `loaded`?
fn same(loaded: &DecisionForm, text: &DecisionForm, images: &[Img]) -> bool {
    loaded.state_mode == text.state_mode
        && loaded.state == text.state
        && loaded.questions == text.questions
        && loaded.images.len() == images.len()
        && loaded.images.iter().zip(images).all(|(l, i)| *l == i.input)
}

impl FormState {
    pub fn new() -> Self {
        let mode = RwSignal::new(StateMode::Text);
        let state = RwSignal::new(String::new());
        let images = RwSignal::new(Vec::<Img>::new());
        let questions = RwSignal::new(Vec::<Q>::new());
        let loaded = RwSignal::new(DecisionForm::default());
        let dirty = Memo::new(move |_| {
            let text = DecisionForm {
                state_mode: mode.get(),
                state: state.get(),
                images: Vec::new(),
                questions: questions.with(|v| v.iter().map(Q::draft).collect()),
            };
            loaded.with(|l| images.with(|i| !same(l, &text, i)))
        });
        Self {
            mode,
            state,
            images,
            questions,
            uids: Uids::new(),
            loaded,
            dirty,
        }
    }

    /// The form as the wire reads it (tracked), each image's data URL
    /// copied: for what goes out (Ask, a count), not what follows each
    /// keystroke.
    pub fn draft(self) -> DecisionForm {
        DecisionForm {
            images: self
                .images
                .with(|v| v.iter().map(|i| i.input.clone()).collect()),
            ..self.text_draft()
        }
    }

    /// The form without its images (tracked): the state and the questions,
    /// all that the check, the count's trigger and the preview's text need.
    pub fn text_draft(self) -> DecisionForm {
        DecisionForm {
            state_mode: self.mode.get(),
            state: self.state.get(),
            images: Vec::new(),
            questions: self.questions.with(|v| v.iter().map(Q::draft).collect()),
        }
    }

    /// The images' row ids (tracked): which images the form holds, for a
    /// memo that must not copy their bytes.
    pub fn image_keys(self) -> Vec<u32> {
        self.images.with(|v| v.iter().map(|i| i.uid).collect())
    }

    pub fn dirty(self) -> bool {
        self.dirty.get()
    }

    /// Replace the form; `artifacts` are the stored group's inputs, which an
    /// `Artifact` image is shown from.
    pub fn load(self, form: DecisionForm, artifacts: &[LabArtifactView]) {
        let uids = self.uids;
        self.mode.set(form.state_mode);
        self.state.set(form.state.clone());
        self.images.set(
            form.images
                .iter()
                .map(|i| image_row(i.clone(), artifacts, uids.next()))
                .collect(),
        );
        self.questions
            .set(rows_of(form.questions.clone(), || uids.next()));
        self.loaded.set(form);
    }

    /// Replace the questions alone (a question set loaded): the state and
    /// the images stay as they are, and the form is dirty afterwards only
    /// where they differ from what was loaded before, or the questions from
    /// these.
    pub fn load_questions(self, drafts: Vec<QuestionDraft>) {
        let uids = self.uids;
        self.questions.set(rows_of(drafts.clone(), || uids.next()));
        self.loaded.update(|l| l.questions = drafts);
    }

    /// The questions as the wire reads them (tracked).
    pub fn question_drafts(self) -> Vec<QuestionDraft> {
        self.questions.with(|v| v.iter().map(Q::draft).collect())
    }

    /// Edit the question `uid` in place.
    pub fn edit(self, uid: u32, f: impl FnOnce(&mut Q)) {
        self.questions.update(|v| {
            if let Some(q) = v.iter_mut().find(|q| q.uid == uid) {
                f(q);
            }
        });
    }
}

/// An image of a loaded form as a row: a data URL shows itself, a stored
/// one its file.
pub(super) fn image_row(input: ImageInput, artifacts: &[LabArtifactView], uid: u32) -> Img {
    match &input {
        ImageInput::DataUrl { url, .. } => Img {
            uid,
            size: data_url_bytes(url),
            src: url.clone(),
            head: None,
            input,
        },
        ImageInput::Artifact { id, .. } => {
            let a = artifacts.iter().find(|a| a.id == *id);
            Img {
                uid,
                size: a.map(|a| a.size).unwrap_or_default(),
                src: a
                    .map(|a| a.href.clone())
                    .unwrap_or_else(|| format!("/labs/api/artifacts/{id}")),
                head: a.and_then(|a| a.data_url_head.clone()),
                input,
            }
        }
    }
}

/// The decoded size of a base64 data URL's payload.
pub(super) fn data_url_bytes(url: &str) -> u64 {
    match url.split_once(',') {
        Some((head, payload)) if head.ends_with(";base64") => {
            (payload.trim_end_matches('=').len() * 3 / 4) as u64
        }
        Some((_, payload)) => payload.len() as u64,
        None => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lmgw_api_types::decision_lab::examples;

    #[test]
    fn rows_give_back_the_drafts_they_were_made_from() {
        for ex in examples() {
            let mut n = 0;
            let rows = rows_of(ex.form.questions.clone(), || {
                n += 1;
                n
            });
            let back: Vec<QuestionDraft> = rows.iter().map(Q::draft).collect();
            assert_eq!(back, ex.form.questions, "{}", ex.name);
            let mut uids: Vec<u32> = rows.iter().map(|q| q.uid).collect();
            uids.extend(rows.iter().flat_map(|q| q.options.iter().map(|o| o.uid)));
            uids.extend(rows.iter().flat_map(|q| q.levels.iter().map(|l| l.uid)));
            let all = uids.len();
            uids.sort_unstable();
            uids.dedup();
            assert_eq!(uids.len(), all, "every row has its own uid");
        }
    }

    #[test]
    fn a_question_keeps_its_options_while_it_is_a_score() {
        let mut n = 0;
        let mut q = new_question(&[], || {
            n += 1;
            n
        });
        q.options[0].name = "billing".into();
        q.kind = Kind::Score;
        assert!(matches!(q.draft().kind, QuestionKind::Score { .. }));
        q.kind = Kind::Choice;
        match q.draft().kind {
            QuestionKind::Choice { options } => assert_eq!(options[0].name, "billing"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_new_question_takes_the_next_free_id() {
        let mut n = 0;
        let mut next = || {
            n += 1;
            n
        };
        let a = new_question(&[], &mut next);
        assert_eq!(a.id, "q1");
        let b = new_question(std::slice::from_ref(&a), &mut next);
        assert_eq!(b.id, "q2");
    }

    #[test]
    fn shift_moves_one_place_and_stops_at_the_ends() {
        let mut v = vec![1, 2, 3];
        shift(&mut v, 0, -1);
        assert_eq!(v, [1, 2, 3]);
        shift(&mut v, 0, 1);
        assert_eq!(v, [2, 1, 3]);
        shift(&mut v, 2, 1);
        assert_eq!(v, [2, 1, 3]);
    }

    #[test]
    fn an_image_is_shown_elided_and_sent_whole_or_by_its_ref() {
        let url = "data:image/png;base64,iVBORw0KGgo=";
        let new = image_row(
            ImageInput::DataUrl {
                name: "a.png".into(),
                url: url.into(),
            },
            &[],
            1,
        );
        assert_eq!(new.shown(), "data:image/png;base64,… 8 B");
        assert_eq!(new.sent(), url);
        let kept = LabArtifactView {
            id: 12,
            run_id: None,
            role: lmgw_api_types::lab_store::LabArtifactRole::Input,
            ord: 0,
            name: "r.png".into(),
            mime: "image/png".into(),
            size: 1_400_000,
            sha256: String::new(),
            data_url_head: Some("data:image/x-receipt;name=r.png;base64".into()),
            href: "/labs/api/artifacts/12".into(),
            created_at: String::new(),
        };
        let input = ImageInput::Artifact {
            id: 12,
            name: "r.png".into(),
        };
        let row = image_row(input.clone(), std::slice::from_ref(&kept), 2);
        assert_eq!(
            row.shown(),
            "data:image/x-receipt;name=r.png;base64,… 1.4 MB",
            "the head it is sent with"
        );
        assert_eq!(row.sent(), "lab-artifact:12", "the server sends the bytes");
        assert_eq!(row.src, "/labs/api/artifacts/12");
        let bare = image_row(input, &[], 3);
        assert_eq!(bare.shown(), "lab-artifact:12");
    }

    #[test]
    fn the_form_is_the_loaded_one_until_a_field_or_an_image_differs() {
        let loaded = examples().into_iter().next().unwrap().form;
        let mut text = loaded.clone();
        assert!(same(&loaded, &text, &[]));
        text.state.push('!');
        assert!(!same(&loaded, &text, &[]));
        let img = image_row(
            ImageInput::Artifact {
                id: 1,
                name: "x".into(),
            },
            &[],
            1,
        );
        assert!(!same(&loaded, &loaded, std::slice::from_ref(&img)));
        let mut with = loaded.clone();
        with.images = vec![img.input.clone()];
        assert!(same(&with, &loaded, &[img]));
    }

    #[test]
    fn a_data_url_counts_its_decoded_bytes() {
        assert_eq!(data_url_bytes("data:image/png;base64,AAAA"), 3);
        assert_eq!(data_url_bytes("data:image/png;base64,AAA="), 2);
        assert_eq!(data_url_bytes("nope"), 0);
    }
}
