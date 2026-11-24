//! docling's `ocr_lang` (BCP-47 tags: `en`, `de-DE`, `zh-Hant`, `mul`) as
//! tesseract's language codes (`eng`, `deu`, `chi_tra`), for the native
//! `ocr_languages` (document-conversion design §7.4.2). docling also takes
//! the three-letter codes themselves (`deu`, `ger`), so a code tesseract
//! knows passes as it is. `mul` means every installed pack: no list, so
//! the native default (the installed packs) applies.

/// `(BCP-47 primary language or language-script, tesseract code)`.
const TAGS: &[(&str, &str)] = &[
    ("af", "afr"),
    ("ar", "ara"),
    ("bg", "bul"),
    ("bn", "ben"),
    ("ca", "cat"),
    ("cs", "ces"),
    ("cy", "cym"),
    ("da", "dan"),
    ("de", "deu"),
    ("el", "ell"),
    ("en", "eng"),
    ("eo", "epo"),
    ("es", "spa"),
    ("et", "est"),
    ("eu", "eus"),
    ("fa", "fas"),
    ("fi", "fin"),
    ("fr", "fra"),
    ("ga", "gle"),
    ("gl", "glg"),
    ("he", "heb"),
    ("hi", "hin"),
    ("hr", "hrv"),
    ("hu", "hun"),
    ("id", "ind"),
    ("is", "isl"),
    ("it", "ita"),
    ("ja", "jpn"),
    ("ko", "kor"),
    ("la", "lat"),
    ("lt", "lit"),
    ("lv", "lav"),
    ("mk", "mkd"),
    ("ms", "msa"),
    ("mt", "mlt"),
    ("nb", "nor"),
    ("nl", "nld"),
    ("nn", "nor"),
    ("no", "nor"),
    ("pl", "pol"),
    ("pt", "por"),
    ("ro", "ron"),
    ("ru", "rus"),
    ("sk", "slk"),
    ("sl", "slv"),
    ("sq", "sqi"),
    ("sr", "srp"),
    ("sv", "swe"),
    ("sw", "swa"),
    ("ta", "tam"),
    ("th", "tha"),
    ("tr", "tur"),
    ("uk", "ukr"),
    ("ur", "urd"),
    ("vi", "vie"),
    ("zh-hans", "chi_sim"),
    ("zh-hant", "chi_tra"),
];

/// Bibliographic codes docling accepts beside the terminological ones.
const BIBLIOGRAPHIC: &[(&str, &str)] = &[
    ("ger", "deu"),
    ("fre", "fra"),
    ("dut", "nld"),
    ("cze", "ces"),
    ("gre", "ell"),
    ("chi", "chi_sim"),
];

fn one(tag: &str) -> Option<String> {
    let t = tag.trim().to_ascii_lowercase().replace('_', "-");
    // Chinese by script, then by region (Taiwan, Hong Kong, Macau write
    // traditional).
    if t == "zh" || t.starts_with("zh-") {
        let traditional =
            t.contains("hant") || ["-tw", "-hk", "-mo"].iter().any(|r| t.ends_with(r));
        return Some(if traditional { "chi_tra" } else { "chi_sim" }.to_string());
    }
    if let Some((_, c)) = BIBLIOGRAPHIC.iter().find(|(b, _)| *b == t) {
        return Some((*c).to_string());
    }
    let primary = t.split('-').next().unwrap_or_default();
    if let Some((_, c)) = TAGS.iter().find(|(p, _)| *p == primary) {
        return Some((*c).to_string());
    }
    // Already a tesseract code (`eng`, `deu`, `chi_sim`).
    if TAGS.iter().any(|(_, c)| *c == tag)
        || tag.len() == 3 && tag.bytes().all(|b| b.is_ascii_lowercase())
    {
        return Some(tag.to_string());
    }
    None
}

/// The tesseract codes for `tags` (`None` for `mul` or no tag: the native
/// default), and the tags no code is known for.
pub fn to_tesseract(tags: &[String]) -> (Option<Vec<String>>, Vec<String>) {
    if tags.is_empty() || tags.iter().any(|t| t == "mul") {
        return (None, Vec::new());
    }
    let mut codes: Vec<String> = Vec::new();
    let mut unknown = Vec::new();
    for t in tags {
        match one(t) {
            Some(c) if !codes.contains(&c) => codes.push(c),
            Some(_) => {}
            None => unknown.push(t.clone()),
        }
    }
    ((!codes.is_empty()).then_some(codes), unknown)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn bcp47_tags_become_tesseract_codes() {
        let (c, u) = to_tesseract(&s(&[
            "en", "de-DE", "zh-Hant", "zh-CN", "ger", "fra", "xx-YY",
        ]));
        assert_eq!(
            c.unwrap(),
            ["eng", "deu", "chi_tra", "chi_sim", "fra"].map(String::from)
        );
        assert_eq!(u, ["xx-YY"]);
        assert_eq!(to_tesseract(&s(&["mul"])), (None, Vec::new()));
    }
}
