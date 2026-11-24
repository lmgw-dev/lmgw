//! Which languages tesseract reads (document-conversion design §7.1, §7.2,
//! WP4), from the request's `ocr_languages` and the packs installed (the
//! probe's `--list-langs`, [`super::super::probe`]).
//!
//! - **Judged when the request is planned** ([`validated`]): each code is a
//!   tesseract pack name by its shape ([`is_code`]), at most
//!   [`MAX_OCR_LANGUAGES`] are named, and a repeat counts once. A request
//!   that breaks a rule is a 400 that names the rule and how many codes
//!   broke it, before any file is read.
//! - **Resolved once per request** ([`Chosen`], kept in the plan): each code
//!   that is installed is read; those that are not are named in one
//!   `ocr_language_missing` warning with the packages that add them, and
//!   recognition goes on with the rest. When none of them is installed,
//!   nothing is read, by name.
//! - Not asked for: the installed packs of the desktop's language (`LANG`)
//!   and English; when neither is installed, every installed pack. The
//!   setting `documents.ocr_languages` of design §4.4 is not built yet.
//!
//! Only installed codes ever reach tesseract's command line. `osd` (the
//! orientation data, [`super::osd`]) and `equ` (equation data) are not
//! languages and are never read with.

use std::sync::{Arc, OnceLock};

use lmgw_api_types::documents::convert::{ConvertWarning, WarningCode};
use lmgw_api_types::documents::{ExtraKind, InstallPlatform};

use super::super::caps::ToolId;
use super::super::tools::{Fact, ToolFacts};
use super::osd::OSD_PACK;

/// Packs `--list-langs` lists that are not languages.
const NOT_LANGUAGES: &[&str] = &[OSD_PACK, "equ"];

/// The most languages one request may name: 32. tesseract tries every
/// language it was given on every word, so a page's time grows with the
/// count, and each loaded pack is 15 MiB or more of its 2 GiB; documents are
/// written in one to three. The 400 for a longer list names this constant.
pub const MAX_OCR_LANGUAGES: usize = 32;

/// The longest a language code may be: 32 characters after an optional
/// `script/` (tesseract's longest pack names, `script/HanS_vert` and
/// `chi_sim_vert`, have 13).
pub const MAX_OCR_LANGUAGE_CHARS: usize = 32;

/// How much of a refused code the 400 quotes.
const SHOWN_CHARS: usize = 40;

/// Whether `code` has the shape of a tesseract pack name: letters, digits
/// and `_`, starting with a letter, two to [`MAX_OCR_LANGUAGE_CHARS`] long,
/// optionally after `script/` (`eng`, `deu`, `chi_sim`, `script/Latin`). A
/// path, an option or white space is never one.
pub fn is_code(code: &str) -> bool {
    let name = code.strip_prefix("script/").unwrap_or(code);
    (2..=MAX_OCR_LANGUAGE_CHARS).contains(&name.len())
        && name.starts_with(|c: char| c.is_ascii_alphabetic())
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// `asked` as the request may ask it (module doc): each code once, in the
/// order asked, or the 400's message.
pub fn validated(asked: &[String]) -> Result<Vec<String>, String> {
    if asked.len() > MAX_OCR_LANGUAGES {
        return Err(format!(
            "ocr_languages names {} codes; at most MAX_OCR_LANGUAGES ({MAX_OCR_LANGUAGES}) are \
             read in one request",
            asked.len()
        ));
    }
    let bad: Vec<&String> = asked.iter().filter(|c| !is_code(c)).collect();
    if let Some(first) = bad.first() {
        let shown: String = first.chars().take(SHOWN_CHARS).collect();
        return Err(format!(
            "ocr_languages: {} of its {} codes are not tesseract language codes (letters, \
             digits and _, at most MAX_OCR_LANGUAGE_CHARS ({MAX_OCR_LANGUAGE_CHARS}) long, as \
             in eng, deu, chi_sim or script/Latin); the first is '{shown}'",
            bad.len(),
            asked.len()
        ));
    }
    // At most MAX_OCR_LANGUAGES entries: a scan per entry is a few compares.
    let mut once: Vec<String> = Vec::with_capacity(asked.len());
    for c in asked {
        if !once.contains(c) {
            once.push(c.clone());
        }
    }
    Ok(once)
}

/// The packs installed, as `--list-langs` gave them.
pub fn installed(facts: &ToolFacts) -> Vec<String> {
    match facts.fact(ToolId::Tesseract) {
        Some(Fact::Found(f)) => f
            .extras
            .iter()
            .filter(|e| e.kind == ExtraKind::Languages)
            .flat_map(|e| e.present.iter().cloned())
            .collect(),
        _ => Vec::new(),
    }
}

/// Whether tesseract's orientation data is installed: the probe read the
/// packs and did not name `osd` among the absent ones.
pub fn osd_installed(facts: &ToolFacts) -> bool {
    match facts.fact(ToolId::Tesseract) {
        Some(Fact::Found(f)) => f
            .extras
            .iter()
            .find(|e| e.kind == ExtraKind::Languages)
            .is_some_and(|e| !e.absent.iter().any(|a| a == OSD_PACK)),
        _ => false,
    }
}

/// The package that holds `code`'s pack on Fedora.
fn fedora_package(code: &str) -> String {
    match code.strip_prefix("script/") {
        Some(script) => format!("tesseract-script-{}", script.to_ascii_lowercase()),
        None => format!("tesseract-langpack-{code}"),
    }
}

/// How the packs of `codes` are installed on `platform`, in words.
pub fn packages(codes: &[&str], platform: InstallPlatform) -> String {
    let names: Vec<String> = codes.iter().map(|c| fedora_package(c)).collect();
    match platform {
        InstallPlatform::Fedora => format!("sudo dnf install {}", names.join(" ")),
        InstallPlatform::ServerImage => format!(
            "build the image with {} in TESSERACT_LANGS, or derive one that installs {}",
            codes.join(" "),
            names.join(" ")
        ),
        _ => format!("install tesseract's {} language data", codes.join(", ")),
    }
}

/// [`packages`] for one code.
pub fn package(code: &str, platform: InstallPlatform) -> String {
    packages(&[code], platform)
}

/// How tesseract's orientation data is installed on `platform`, in words.
pub fn osd_hint(platform: InstallPlatform) -> String {
    match platform {
        InstallPlatform::Fedora => format!("sudo dnf install {}", super::osd::OSD_PACKAGE),
        InstallPlatform::ServerImage => {
            format!("derive an image that installs {}", super::osd::OSD_PACKAGE)
        }
        _ => "install tesseract's osd.traineddata".to_string(),
    }
}

/// Settings › Documents' row for tesseract's orientation data when
/// tesseract is found without it: what it adds (rotated scans) and the
/// package. Nothing while tesseract itself is missing (its own row says
/// what to install) or the data is there.
pub fn osd_missing(
    facts: &ToolFacts,
    platform: InstallPlatform,
) -> Option<lmgw_api_types::documents::MissingRow> {
    use lmgw_api_types::documents::{HintKind, InstallHint, MissingRow};
    let Some(Fact::Found(_)) = facts.fact(ToolId::Tesseract) else {
        return None;
    };
    if osd_installed(facts) {
        return None;
    }
    let (kind, text) = match platform {
        InstallPlatform::Fedora => (HintKind::Command, osd_hint(platform)),
        InstallPlatform::ServerImage => (
            HintKind::Command,
            format!(
                "FROM localhost/lmgw-server:current\nRUN dnf -y install \
                 --setopt=install_weak_deps=False {} && dnf clean all",
                super::osd::OSD_PACKAGE
            ),
        ),
        _ => (
            HintKind::Manual,
            "Install tesseract's orientation data (osd.traineddata) with tesseract's language \
             packs."
                .to_string(),
        ),
    };
    Some(MissingRow {
        tool: super::osd::OSD_PACKAGE.to_string(),
        label: "Page orientation for text recognition".to_string(),
        install: vec![InstallHint {
            platform,
            kind,
            text,
            note: Some(OSD_MISSING_NOTE.to_string()),
        }],
        gives: vec!["rotated scans".to_string()],
        licence: "Apache-2.0".to_string(),
    })
}

/// What the Settings row says a machine without the orientation data does
/// with a turned page (measured 2026-11-19, tesseract 5.5.4; `pages.rs`
/// words the warnings).
pub const OSD_MISSING_NOTE: &str =
    "Without it, a page scanned on its side is turned by a guess from the shape of its lines \
     and read again, and a page scanned upside down is read as it is, with a warning that its \
     text may be wrong.";

/// The desktop's language as a tesseract code, from `LC_ALL`,
/// `LC_MESSAGES` or `LANG` (`de_DE.UTF-8` → `deu`).
fn locale() -> Option<String> {
    let tag = ["LC_ALL", "LC_MESSAGES", "LANG"]
        .iter()
        .filter_map(|v| std::env::var(v).ok())
        .find(|v| !v.is_empty())?;
    let tag = tag.split(['.', '@']).next().unwrap_or_default().to_string();
    if tag == "C" || tag == "POSIX" {
        return None;
    }
    super::super::docling_serve::lang::to_tesseract(&[tag])
        .0
        .and_then(|c| c.into_iter().next())
}

/// What a request's text recognition reads: tesseract's `-l` (`eng+deu`)
/// with the warning for what was asked for and is not installed, or why
/// nothing can be read.
pub type Choice = Result<(String, Vec<ConvertWarning>), String>;

/// A request's [`Choice`], made by the first of its files that needs it and
/// shared by the rest ([`super::super::options::Plan::ocr_chosen`]).
#[derive(Debug, Clone, Default)]
pub struct Chosen(Arc<OnceLock<Choice>>);

impl Chosen {
    /// The request's choice: made now if no file has asked yet.
    pub fn get(
        &self,
        asked: Option<&[String]>,
        facts: &ToolFacts,
        platform: InstallPlatform,
    ) -> &Choice {
        self.0
            .get_or_init(|| choose(asked, &installed(facts), platform))
    }
}

/// Two plans are the same plan whatever their requests have resolved.
impl PartialEq for Chosen {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}

impl Eq for Chosen {}

/// The languages read, as tesseract's `-l` (`eng+deu`), with the warning
/// for those asked for and not installed; `Err` when none can be read.
pub fn choose(asked: Option<&[String]>, installed: &[String], platform: InstallPlatform) -> Choice {
    choose_for(asked, installed, platform, locale().as_deref())
}

fn choose_for(
    asked: Option<&[String]>,
    installed: &[String],
    platform: InstallPlatform,
    locale: Option<&str>,
) -> Choice {
    let langs: Vec<&str> = installed
        .iter()
        .map(String::as_str)
        .filter(|l| !NOT_LANGUAGES.contains(l))
        .collect();
    let have: std::collections::HashSet<&str> = langs.iter().copied().collect();
    let Some(asked) = asked.filter(|a| !a.is_empty()) else {
        let mut picked: Vec<&str> = Vec::new();
        for c in locale.into_iter().chain(["eng"]) {
            if have.contains(c) && !picked.contains(&c) {
                picked.push(c);
            }
        }
        if picked.is_empty() {
            picked = langs.clone();
        }
        if picked.is_empty() {
            return Err(format!(
                "tesseract has no language pack installed ({})",
                package("eng", platform)
            ));
        }
        return Ok((picked.join("+"), Vec::new()));
    };
    // `asked` is validated: each code once, at most MAX_OCR_LANGUAGES.
    let (read, missing): (Vec<&str>, Vec<&str>) = asked
        .iter()
        .map(String::as_str)
        .partition(|c| have.contains(c));
    if read.is_empty() {
        return Err(format!(
            "none of the languages ocr_languages asks for ({}) is installed for tesseract \
             (installed: {}); {}",
            missing.join(", "),
            if langs.is_empty() {
                "none".to_string()
            } else {
                langs.join(", ")
            },
            packages(&missing, platform)
        ));
    }
    let mut warnings = Vec::new();
    if !missing.is_empty() {
        let mut w = super::warning(
            WarningCode::OcrLanguageMissing,
            format!(
                "ocr_languages asks for {}, which tesseract has no pack for here ({} adds {}); \
                 text was recognised as {}",
                missing.join(", "),
                packages(&missing, platform),
                if missing.len() == 1 { "it" } else { "them" },
                read.join(", ")
            ),
            Vec::new(),
        );
        w.option = Some("ocr_languages".to_string());
        warnings.push(w);
    }
    Ok((read.join("+"), warnings))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn the_default_is_the_desktops_language_and_english() {
        let inst = s(&["deu", "eng", "fra", "osd"]);
        let f = InstallPlatform::Fedora;
        assert_eq!(
            choose_for(None, &inst, f, Some("deu")).unwrap().0,
            "deu+eng"
        );
        assert_eq!(choose_for(None, &inst, f, None).unwrap().0, "eng");
        assert_eq!(
            choose_for(None, &s(&["fra", "osd"]), f, None).unwrap().0,
            "fra"
        );
        let e = choose_for(None, &s(&["osd"]), f, None).unwrap_err();
        assert!(e.contains("tesseract-langpack-eng"), "{e}");
    }

    #[test]
    fn languages_not_installed_are_named_in_one_warning_with_their_packages() {
        let inst = s(&["deu", "eng", "osd"]);
        let f = InstallPlatform::Fedora;
        let (l, w) = choose_for(Some(&s(&["fra", "deu", "ita"])), &inst, f, None).unwrap();
        assert_eq!(l, "deu");
        assert_eq!(w.len(), 1, "one warning for every missing code");
        assert_eq!(w[0].code, WarningCode::OcrLanguageMissing);
        assert!(
            w[0].message
                .contains("sudo dnf install tesseract-langpack-fra tesseract-langpack-ita"),
            "{}",
            w[0].message
        );
        assert!(
            w[0].message.contains("asks for fra, ita")
                && w[0].message.contains("recognised as deu"),
            "{}",
            w[0].message
        );
        let e = choose_for(Some(&s(&["fra", "osd"])), &inst, f, None).unwrap_err();
        assert!(
            e.contains("fra, osd") && e.contains("installed: deu, eng"),
            "{e}"
        );
        assert!(package("script/Latin", f).ends_with("tesseract-script-latin"));
        assert!(package("fra", InstallPlatform::ServerImage).contains("TESSERACT_LANGS"));
    }

    #[test]
    fn a_code_has_a_pack_names_shape_and_a_list_its_bound() {
        for ok in ["eng", "deu", "chi_sim", "script/Latin", "deu_frak", "osd"] {
            assert!(is_code(ok), "{ok}");
        }
        for bad in [
            "",
            "e",
            "../../x",
            "eng deu",
            "eng+deu",
            "-l",
            "script/",
            "script/../x",
            "_eng",
            "9ab",
            "eng\n",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        ] {
            assert!(!is_code(bad), "{bad:?}");
        }
        assert_eq!(
            validated(&s(&["deu", "eng", "deu"])).unwrap(),
            s(&["deu", "eng"])
        );
        let e = validated(&s(&["eng", "../../x", "a b"])).unwrap_err();
        assert!(
            e.contains("2 of its 3 codes") && e.contains("'../../x'"),
            "{e}"
        );
    }

    /// The reviewed request: a million distinct codes. Refused by its count
    /// before any code is looked at, in no time.
    #[test]
    fn a_million_codes_are_refused_by_their_count() {
        let many: Vec<String> = (0..1_000_000).map(|i| format!("l{i:06}")).collect();
        let started = std::time::Instant::now();
        let e = validated(&many).unwrap_err();
        assert!(
            e.contains("1000000 codes") && e.contains("MAX_OCR_LANGUAGES (32)"),
            "{e}"
        );
        assert!(
            started.elapsed().as_millis() < 100,
            "{:?}",
            started.elapsed()
        );
        // One past the bound is refused, the bound itself read.
        let at: Vec<String> = (0..MAX_OCR_LANGUAGES).map(|i| format!("l{i:02}")).collect();
        assert_eq!(validated(&at).unwrap().len(), MAX_OCR_LANGUAGES);
        let mut past = at;
        past.push("eng".into());
        assert!(validated(&past).is_err());
    }

    #[test]
    fn a_requests_choice_is_made_once() {
        let chosen = Chosen::default();
        let facts = ToolFacts::all_missing();
        let a = chosen.get(None, &facts, InstallPlatform::Fedora) as *const Choice;
        let b = chosen
            .clone()
            .get(Some(&s(&["deu"])), &facts, InstallPlatform::Fedora)
            as *const Choice;
        assert_eq!(a, b, "the second file reads the first one's choice");
    }
}
