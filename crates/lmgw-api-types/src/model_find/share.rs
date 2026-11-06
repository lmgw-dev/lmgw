//! "Send this model to a friend" (find-a-model design §7, D31, D124): the
//! file's public Hugging Face address and the sentence around it, written
//! once here for the server's card (`ModelCard.share`) and the dashboard's
//! card, which sends the file it shows (WP7, D101, D109). Pure.

use super::ModelCard;

/// The hub the address points at.
const HUB: &str = "https://huggingface.co";

/// One path segment, every byte but RFC 3986's unreserved ones (letters,
/// digits, `-._~`) percent-encoded: a space, `#`, `?`, `%`, a bracket, a
/// quote or `|` in a name survives, and so does the address inside a
/// sentence, whose reader stops at brackets and quotes.
fn segment(s: &str, out: &mut String) {
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            out.push(b as char);
        } else {
            out.push('%');
            out.push(char::from(b"0123456789ABCDEF"[usize::from(b >> 4)]));
            out.push(char::from(b"0123456789ABCDEF"[usize::from(b & 15)]));
        }
    }
}

/// `path`'s segments (split on `/`), each encoded, joined by `/`.
fn path(p: &str, out: &mut String) {
    for (i, seg) in p.split('/').enumerate() {
        if i > 0 {
            out.push('/');
        }
        segment(seg, out);
    }
}

/// The file's page on the hub, at `main`, not a commit (§7):
/// `https://huggingface.co/<upload>/blob/main/<file>`.
pub fn share_url(upload: &str, file: &str) -> String {
    let mut out = format!("{HUB}/");
    path(upload, &mut out);
    out.push_str("/blob/main/");
    path(file, &mut out);
    out
}

/// The text "Send to a friend" copies (§7): the model's name and the
/// file's address, and nothing else.
pub fn share_text(name: &str, url: &str) -> String {
    format!("Get {name} in lmgw: open Models \u{2192} Find a model and paste\n{url}")
}

/// A repo id as a display name: its name with hyphens as spaces
/// (`maker-demo/Demo-2-8B-it` → "Demo 2 8B it").
pub fn display_name(id: &str) -> String {
    id.split_once('/').map_or(id, |(_, n)| n).replace('-', " ")
}

/// The name a share text gives `card`'s model sent from `upload`: the
/// card's display name, unless the original is private to the account,
/// whose name is then not the friend's to read: the upload's own name,
/// less a trailing `-GGUF` (WP7, D109).
pub fn share_name(card: &ModelCard, upload: &str) -> String {
    if !card.origin_private {
        return card.name.clone();
    }
    let name = upload.split_once('/').map_or(upload, |(_, n)| n);
    let bare = match name.len().checked_sub(5) {
        Some(cut)
            if name.is_char_boundary(cut)
                && name[cut..].eq_ignore_ascii_case("-gguf")
                && cut > 0 =>
        {
            &name[..cut]
        }
        _ => name,
    };
    bare.replace('-', " ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_address_encodes_every_byte_but_the_unreserved_ones() {
        assert_eq!(
            share_url("fan-demo/Demo-8B-GGUF", "Demo-8B-Q4_K_M.gguf"),
            "https://huggingface.co/fan-demo/Demo-8B-GGUF/blob/main/Demo-8B-Q4_K_M.gguf"
        );
        assert_eq!(
            share_url("fan-demo/Demo-GGUF", "sub dir/A b#c?.gguf"),
            "https://huggingface.co/fan-demo/Demo-GGUF/blob/main/sub%20dir/A%20b%23c%3F.gguf"
        );
        assert_eq!(
            share_url("fan-demo/Demo-GGUF", "Model[Q4].gguf"),
            "https://huggingface.co/fan-demo/Demo-GGUF/blob/main/Model%5BQ4%5D.gguf"
        );
        assert_eq!(
            share_url("fan-demo/Demo-GGUF", "a^b|c(d)'e\"f<g>`h%i{j}.gguf"),
            "https://huggingface.co/fan-demo/Demo-GGUF/blob/main/\
             a%5Eb%7Cc%28d%29%27e%22f%3Cg%3E%60h%25i%7Bj%7D.gguf"
        );
        assert_eq!(
            share_url("fan-demo/Demo-GGUF", "Démo~1.gguf"),
            "https://huggingface.co/fan-demo/Demo-GGUF/blob/main/D%C3%A9mo~1.gguf"
        );
    }

    #[test]
    fn the_text_is_the_name_and_the_address() {
        assert_eq!(
            share_text("Demo 8B", "https://huggingface.co/a/b/blob/main/c.gguf"),
            "Get Demo 8B in lmgw: open Models \u{2192} Find a model and paste\n\
             https://huggingface.co/a/b/blob/main/c.gguf"
        );
    }

    #[test]
    fn a_private_originals_model_is_named_by_its_upload() {
        let mut c = ModelCard {
            name: "Secret Demo 8B".into(),
            ..ModelCard::default()
        };
        assert_eq!(share_name(&c, "fan-demo/Demo-8B-GGUF"), "Secret Demo 8B");
        c.origin_private = true;
        assert_eq!(share_name(&c, "fan-demo/Demo-8B-GGUF"), "Demo 8B");
        assert_eq!(share_name(&c, "fan-demo/Demo-8B-gguf"), "Demo 8B");
        assert_eq!(share_name(&c, "fan-demo/Demo-8B"), "Demo 8B");
        assert_eq!(display_name("maker-demo/Demo-2-8B-it"), "Demo 2 8B it");
    }
}
