//! "Send to a friend" (find-a-model design §7, D31, D124, WP7): the text the
//! card copies and the card's two states around it. The text is the
//! server's (`lmgw_api_types::model_find::share_text` and `share_url`, D109),
//! built here from the card's public fields only (its name, the upload and
//! the file the card shows), so a file chosen by hand or the other measure's
//! pick is the one that is sent, not the one the answer started with.
//! Pure, so the rules are unit-tested without a browser.

use leptos::prelude::*;
use lmgw_api_types::model_find::{share_name, share_text, share_url, ModelCard};

use super::shown::Shown;
use super::Finder;

/// The sentence a private repo's card shows in place of the button (D124).
pub(super) const PRIVATE_LINE: &str =
    "This model is private to your Hugging Face account, so there is no link to share.";

/// What the card offers for the file it shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Share {
    /// The text to copy.
    Text(String),
    /// The repo is private to the account: no link (D124).
    Private,
    /// Nothing: no file, or a repo the card has no word on (a file picked
    /// by hand from an upload the card didn't list), so lmgw can't tell
    /// that its address is public.
    Nothing,
}

/// What the card offers for `shown`'s file.
pub(super) fn share_for(card: &ModelCard, shown: &Shown) -> Share {
    if shown.upload.is_empty() || shown.file.is_empty() {
        return Share::Nothing;
    }
    let private = if shown.upload == card.upload {
        card.private
    } else {
        match card.other_uploads.iter().find(|u| u.repo == shown.upload) {
            Some(u) => u.private,
            None => return Share::Nothing,
        }
    };
    if private {
        Share::Private
    } else {
        Share::Text(share_text(
            &share_name(card, &shown.upload),
            &share_url(&shown.upload, &shown.file),
        ))
    }
}

/// "Send to a friend": copies the text and says so in a toast, or why it
/// didn't.
#[component]
pub(super) fn SendToFriend(
    f: Finder,
    card: StoredValue<ModelCard>,
    shown: Memo<Shown>,
) -> impl IntoView {
    move || {
        let Share::Text(text) = card.with_value(|c| shown.with(|s| share_for(c, s))) else {
            return None;
        };
        let toasts = f.toasts;
        let copy = move |_| {
            let text = text.clone();
            leptos::task::spawn_local(async move {
                match lmgw_ui_kit::clipboard::write_text(&text).await {
                    Ok(()) => toasts.ok("Copied. Paste it in a message to your friend."),
                    Err(why) => toasts.err(format!("Not copied: {why}.")),
                }
            });
        };
        Some(view! {
            <button
                class="btn"
                title="Copies the model's name and its Hugging Face address, to paste into lmgw's Find box"
                on:click=copy
            >
                "Send to a friend"
            </button>
        })
    }
}

/// D124's sentence, where the button would be.
#[component]
pub(super) fn PrivateLine(card: StoredValue<ModelCard>, shown: Memo<Shown>) -> impl IntoView {
    move || {
        (card.with_value(|c| shown.with(|s| share_for(c, s))) == Share::Private)
            .then(|| view! { <p class="mf-line dim mf-private">{PRIVATE_LINE}</p> })
    }
}

#[cfg(test)]
mod tests {
    use lmgw_api_types::model_find::{
        BesideTerms, CardFile, CardMeasure, DownloadEstimate, FileFit, FilePick, FitMark,
        FitMeasure, PickRule, PlanFit, ShareLink, UploadChoice,
    };
    use lmgw_api_types::{HfCompanion, ModelLicence, RepoFile, RepoFiles, RepoPicks};

    use super::super::shown::{self, Choice};
    use super::*;

    /// A token-looking value, as a Hugging Face token reads.
    const TOKEN: &str = "hf_DemoTokenDemoTokenDemoToken0000";

    /// A fit full of numbers no share text has.
    fn fit(measure: FitMeasure) -> FileFit {
        FileFit {
            mark: FitMark::Fits,
            measure,
            ctx: Some(131_071),
            trained_ctx: Some(262_143),
            uses_bytes: 987_654_321,
            capacity_bytes: 23_456_789_012,
            headroom_bytes: 536_870_913,
            beside: Some(BesideTerms {
                outside_bytes: Some(1_111_111_111),
                residents_bytes: 2_222_222_222,
            }),
            bits_per_weight: Some(4.8765),
            parameters: Some(8_123_456_789),
            notes: vec![format!("note-demo {TOKEN}")],
            ..FileFit::default()
        }
    }

    /// A card with a distinct `*-demo` value, a token and odd numbers in
    /// every field that isn't public, and a private other upload beside a
    /// public one.
    fn card() -> ModelCard {
        ModelCard {
            name: "Demo 8B".into(),
            by: "maker-demo".into(),
            by_user: Some("person-demo".into()),
            origin: "origin-demo/Demo-8B".into(),
            upload: "fan-demo/Demo-8B-GGUF".into(),
            file: "Demo-8B-Q4_K_M.gguf".into(),
            quant: Some("Q4_K_M".into()),
            companions: vec![HfCompanion {
                file: "mmproj-companion-demo.gguf".into(),
                role: "mmproj".into(),
            }],
            can: vec!["can-demo".into()],
            parameters: Some(8_123_456_789),
            licence: Some(ModelLicence {
                id: Some("licence-demo".into()),
                name: Some("licence-name-demo".into()),
                link: Some("https://licence-demo.example/".into()),
            }),
            download: DownloadEstimate {
                files_bytes: 5_432_109_876,
                engine_bytes: Some(3_210_987_654),
                rate_bytes_per_sec: Some(12_345_678.9),
                secs: Some(4_321),
                sentence: Some("download-demo".into()),
                notes: vec!["download-note-demo".into()],
                ..DownloadEstimate::default()
            },
            disk_free_bytes: Some(765_432_109_876),
            fit: fit(FitMeasure::Beside),
            whole_card: Some(CardFile {
                file: "Demo-8B-Q8_0-wholecard-demo.gguf".into(),
                quant: Some("Q8_0".into()),
                bytes: 8_765_432_109,
                fit: fit(FitMeasure::WholeCard),
            }),
            why: format!("why-demo: planned ctx 131072 on http://127.0.0.1:8001 {TOKEN}"),
            // The server's text is the file the answer started with; the
            // card's text is built from what it shows.
            share: Some(ShareLink {
                url: "https://share-demo.example/".into(),
                text: format!("share-demo {TOKEN}"),
            }),
            other_uploads: vec![
                UploadChoice {
                    repo: "open-demo/Demo-8B-GGUF".into(),
                    ..UploadChoice::default()
                },
                UploadChoice {
                    repo: "closed-demo/Demo-8B-GGUF".into(),
                    private: true,
                    ..UploadChoice::default()
                },
            ],
            ..ModelCard::default()
        }
    }

    /// The header's marks for the card's upload: the beside pick and the
    /// whole card's, each fit full of numbers, the card measured with a
    /// resident.
    fn marks() -> RepoFiles {
        let plan = || PlanFit {
            card: None,
            beside: fit(FitMeasure::Beside),
            whole_card: fit(FitMeasure::WholeCard),
        };
        let pick = |file: &str, measure| FilePick {
            file: Some(file.into()),
            rule: PickRule::WholeContext,
            measure,
            why: format!("pick-why-demo {TOKEN}"),
            full_context: None,
        };
        RepoFiles {
            repo: "fan-demo/Demo-8B-GGUF".into(),
            files: vec![
                RepoFile {
                    file: "Demo-8B-Q4_K_M.gguf".into(),
                    role: "weights".into(),
                    size_bytes: 4_987_654_321,
                    fit: Some(plan()),
                    ..RepoFile::default()
                },
                RepoFile {
                    file: "Demo-8B-Q8_0-wholecard-demo.gguf".into(),
                    role: "weights".into(),
                    size_bytes: 8_765_432_109,
                    fit: Some(plan()),
                    ..RepoFile::default()
                },
            ],
            recommended: Some("Demo-8B-Q4_K_M.gguf".into()),
            picks: Some(RepoPicks {
                beside: pick("Demo-8B-Q4_K_M.gguf", FitMeasure::Beside),
                whole_card: pick("Demo-8B-Q8_0-wholecard-demo.gguf", FitMeasure::WholeCard),
            }),
            card: Some(CardMeasure {
                capacity_bytes: 25_769_803_777,
                headroom_bytes: 536_870_913,
                outside_bytes: Some(1_111_111_111),
                ..CardMeasure::default()
            }),
            ..RepoFiles::default()
        }
    }

    /// Every value of `card()` and `marks()` that isn't public.
    const NOT_PUBLIC: &[&str] = &[
        "maker-demo",
        "person-demo",
        "origin-demo",
        "companion-demo",
        "can-demo",
        "licence-demo",
        "licence-name-demo",
        "download-demo",
        "download-note-demo",
        "why-demo",
        "pick-why-demo",
        "note-demo",
        "share-demo",
        "wholecard-demo",
        "closed-demo",
        TOKEN,
        "hf_",
        "127.0.0.1",
        "8001",
        "131072",
        "131071",
        "262143",
        "987654321",
        "23456789012",
        "25769803777",
        "536870913",
        "1111111111",
        "2222222222",
        "8123456789",
        "4987654321",
        "8765432109",
        "5432109876",
        "3210987654",
        "765432109876",
        "4321",
        "4.8765",
        "http://",
    ];

    fn none_in(what: &str, out: &str) {
        for private in NOT_PUBLIC {
            assert!(!out.contains(private), "{what}: {private:?} in {out:?}");
        }
    }

    #[test]
    fn the_text_is_the_name_and_the_shown_files_public_address() {
        let c = card();
        assert_eq!(
            share_for(&c, &shown::shown(&c, None, &Choice::Pick)),
            Share::Text(
                "Get Demo 8B in lmgw: open Models \u{2192} Find a model and paste\n\
                 https://huggingface.co/fan-demo/Demo-8B-GGUF/blob/main/Demo-8B-Q4_K_M.gguf"
                    .into()
            )
        );
    }

    /// Every branch of `share_for`, on a card and marks full of values that
    /// aren't public: none of them is in what the card offers.
    #[test]
    fn nothing_that_isnt_public_is_in_any_answer() {
        let c = card();
        let m = marks();
        let pick = shown::shown(&c, Some(&m), &Choice::Pick);
        assert!(pick.exact, "built from the marks");
        assert_eq!(pick.fit.capacity_bytes, 23_456_789_012);
        let mut cases = vec![
            ("before the marks", shown::shown(&c, None, &Choice::Pick)),
            ("from the marks", pick.clone()),
        ];
        // The other public upload, while the private one is listed too.
        let mut open = pick.clone();
        open.upload = "open-demo/Demo-8B-GGUF".into();
        cases.push(("another public upload", open));
        for (what, s) in &cases {
            let Share::Text(t) = share_for(&c, s) else {
                panic!("{what}: a public file has a text");
            };
            none_in(what, &t);
            assert!(t.starts_with("Get Demo 8B in lmgw"), "{what}: {t}");
            assert!(t.ends_with("/blob/main/Demo-8B-Q4_K_M.gguf"), "{what}: {t}");
        }
        // The whole card's pick is public once it is the file shown.
        let other = shown::shown(&c, Some(&m), &Choice::Other);
        assert_eq!(other.file, "Demo-8B-Q8_0-wholecard-demo.gguf");
        assert!(
            matches!(share_for(&c, &other), Share::Text(t) if t.ends_with("wholecard-demo.gguf"))
        );
        // The private upload, a private card, an unlisted upload and no
        // file: no text, and nothing of the card in what is offered.
        let mut closed = pick.clone();
        closed.upload = "closed-demo/Demo-8B-GGUF".into();
        let mut private = c.clone();
        private.private = true;
        let mut unlisted = pick.clone();
        unlisted.upload = "unlisted-demo/Demo-8B-GGUF".into();
        let mut no_file = pick.clone();
        no_file.file.clear();
        for (what, card, s, want) in [
            ("a private other upload", &c, &closed, Share::Private),
            ("a private card", &private, &pick, Share::Private),
            ("an unlisted upload", &c, &unlisted, Share::Nothing),
            ("no file", &c, &no_file, Share::Nothing),
        ] {
            let got = share_for(card, s);
            assert_eq!(got, want, "{what}");
            none_in(what, &format!("{got:?}"));
        }
    }

    /// A public upload of an original private to the account: the share
    /// text names the model by the upload, not by the original (D109).
    #[test]
    fn a_private_originals_name_isnt_sent() {
        let mut c = card();
        c.name = "Secret Origin Demo".into();
        c.origin_private = true;
        let mut s = shown::shown(&c, None, &Choice::Pick);
        s.upload = "open-demo/Demo-8B-GGUF".into();
        let Share::Text(t) = share_for(&c, &s) else {
            panic!("a public upload has a text");
        };
        assert!(t.starts_with("Get Demo 8B in lmgw"), "{t}");
        assert!(!t.contains("Secret"), "{t}");
    }

    #[test]
    fn a_private_repo_has_a_sentence_and_no_text() {
        let mut c = card();
        c.private = true;
        assert_eq!(
            share_for(&c, &shown::shown(&c, None, &Choice::Pick)),
            Share::Private
        );
        assert!(PRIVATE_LINE.starts_with("This model is private to your Hugging Face account"));
    }
}
