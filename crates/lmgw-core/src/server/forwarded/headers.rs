//! The forwarded headers as text (headless server design §4.5): the hops of
//! `X-Forwarded-For`, the one value of `X-Forwarded-Proto` and
//! `X-Forwarded-Host`, and RFC 7239's `Forwarded` elements.

use std::net::{IpAddr, SocketAddr};

use axum::http::{HeaderMap, HeaderName};

use crate::net::Scheme;

/// One hop of a forwarded chain: an address (its port 0 when the hop names
/// none), or a hop that names no address — RFC 7239's `unknown`, an
/// obfuscated identifier (`_hidden`), or text that is no address at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Hop {
    Addr(SocketAddr),
    Unknown,
}

/// A hop's text read: `192.0.2.5`, `192.0.2.5:4711`, `2001:db8::5`,
/// `[2001:db8::5]`, `[2001:db8::5]:4711`, the port possibly obfuscated
/// (`192.0.2.5:_p`, read as port 0); anything else is [`Hop::Unknown`].
pub(super) fn hop(text: &str) -> Hop {
    let t = text.trim().trim_matches('"').trim();
    if let Ok(ip) = t.parse::<IpAddr>() {
        return Hop::Addr(SocketAddr::new(ip.to_canonical(), 0));
    }
    if let Ok(a) = t.parse::<SocketAddr>() {
        return Hop::Addr(SocketAddr::new(a.ip().to_canonical(), a.port()));
    }
    // A bracketed IPv6 address without a port, or either form with an
    // obfuscated port.
    let (host, port) = match t.strip_prefix('[') {
        Some(rest) => match rest.split_once(']') {
            Some((h, "")) => (h, None),
            Some((h, p)) => match p.strip_prefix(':') {
                Some(p) => (h, Some(p)),
                None => return Hop::Unknown,
            },
            None => return Hop::Unknown,
        },
        None => match t.rsplit_once(':') {
            Some((h, p)) if !h.contains(':') => (h, Some(p)),
            _ => return Hop::Unknown,
        },
    };
    let Ok(ip) = host.parse::<IpAddr>() else {
        return Hop::Unknown;
    };
    let port = match port {
        None => 0,
        Some(p) if p.starts_with('_') => 0,
        Some(p) => match p.parse::<u16>() {
            Ok(p) => p,
            Err(_) => return Hop::Unknown,
        },
    };
    Hop::Addr(SocketAddr::new(ip.to_canonical(), port))
}

/// Every value of the list header `name`, across all its lines, in order,
/// split on commas and trimmed; empty list elements dropped, as a list's
/// recipient must accept them (RFC 9110 §5.6.1). Split as bytes, so one
/// element that is not text (`None`) leaves the others readable: in
/// `X-Forwarded-For` it is a hop that names no address, and left of the
/// client it is the client's own text, which refuses nothing.
pub(super) fn list(headers: &HeaderMap, name: &HeaderName) -> Vec<Option<String>> {
    headers
        .get_all(name)
        .iter()
        .flat_map(|v| v.as_bytes().split(|b| *b == b','))
        .map(|e| e.trim_ascii())
        .filter(|e| !e.is_empty())
        .map(|e| std::str::from_utf8(e).ok().map(str::to_string))
        .collect()
}

/// The one value of `name`, a header a proxy sets rather than appends to
/// (`X-Forwarded-Proto`, `X-Forwarded-Host`): `None` when it is absent; its
/// value when every value it carries is the same one (a chain of proxies
/// that each appended what they agree on); refused, naming the values,
/// when they differ: which hop's to believe is not lmgw's to guess.
pub(super) fn single(headers: &HeaderMap, name: &HeaderName) -> Result<Option<String>, String> {
    let Some(values) = list(headers, name).into_iter().collect::<Option<Vec<_>>>() else {
        return Err(format!("{} carries a value that is not text", shown(name)));
    };
    let Some(first) = values.first() else {
        return Ok(None);
    };
    if values.iter().any(|v| !v.eq_ignore_ascii_case(first)) {
        return Err(format!(
            "{} carries values that differ ({}): a proxy in front of lmgw must set one value, not \
             add to the client's",
            shown(name),
            values.join(", ")
        ));
    }
    Ok(Some(first.clone()))
}

/// A header's name as its specification writes it (`X-Forwarded-Proto`):
/// each dash-separated word capitalised.
fn shown(name: &HeaderName) -> String {
    name.as_str()
        .split('-')
        .map(|w| {
            let mut c = w.chars();
            match c.next() {
                Some(f) => f.to_ascii_uppercase().to_string() + c.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join("-")
}

/// A forwarded protocol read: `http` or `https`, and the WebSocket schemes
/// some proxies write for an upgrade (`ws` is `http`, `wss` is `https`).
pub(super) fn proto(text: &str, from: &str) -> Result<Scheme, String> {
    match text.trim().to_ascii_lowercase().as_str() {
        "http" | "ws" => Ok(Scheme::Http),
        "https" | "wss" => Ok(Scheme::Https),
        other => Err(format!(
            "{from} names the scheme '{other}': lmgw reads http or https (ws and wss for a \
             WebSocket)"
        )),
    }
}

/// A forwarded host read: an authority (`lmgw.example`,
/// `lmgw.example:8443`, `[2001:db8::5]:8443`) without user information.
pub(super) fn host(text: &str, from: &str) -> Result<String, String> {
    let t = text.trim();
    let bad = || {
        Err(format!(
            "{from} names the host '{t}': lmgw reads a host and an optional port \
             (lmgw.example:8443)"
        ))
    };
    if t.is_empty() || t.contains('@') {
        return bad();
    }
    let Ok(a) = t.parse::<axum::http::uri::Authority>() else {
        return bad();
    };
    // Rebuilt from its parts, so a port that is not a number (which the
    // parser leaves to `port_u16`) and anything past the authority are
    // refused rather than carried.
    let rebuilt = match a.port_u16() {
        Some(p) => format!("{}:{p}", a.host()),
        None => a.host().to_string(),
    };
    if a.host().is_empty() || rebuilt != t {
        return bad();
    }
    Ok(t.to_string())
}

/// One element of RFC 7239's `Forwarded`: what one proxy said of the
/// request it received. `by` and the extensions are not read.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct Element {
    pub for_: Option<String>,
    pub proto: Option<String>,
    pub host: Option<String>,
}

/// `Forwarded`'s elements, every line in order (RFC 7239 §4): a list of
/// elements, each a `;`-separated list of `name=value` pairs whose value is
/// a token or a quoted string. Names are case-insensitive; a parameter given
/// twice in one element is refused (§4: it "MUST NOT occur more than once
/// per forwarded-element"), and so is text that does not parse, since its
/// elements cannot be told apart.
pub(super) fn forwarded(headers: &HeaderMap) -> Result<Vec<Element>, String> {
    let mut out = Vec::new();
    for v in headers.get_all(axum::http::header::FORWARDED) {
        // A quoted string may carry obs-text (RFC 9110 §5.6.4); read
        // lossily, such a value is no address, and anywhere else the
        // replacement character does not parse.
        let v = String::from_utf8_lossy(v.as_bytes());
        parse_elements(&v, &mut out).map_err(|why| {
            format!("Forwarded does not parse as RFC 7239 writes it ({why}): '{v}'")
        })?;
    }
    Ok(out)
}

/// RFC 7230's `tchar`.
fn tchar(c: char) -> bool {
    c.is_ascii_alphanumeric() || "!#$%&'*+-.^_`|~".contains(c)
}

fn parse_elements(text: &str, out: &mut Vec<Element>) -> Result<(), String> {
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    let skip_ows = |i: &mut usize| {
        while *i < chars.len() && (chars[*i] == ' ' || chars[*i] == '\t') {
            *i += 1;
        }
    };
    let mut element = Element::default();
    let mut seen: Vec<String> = Vec::new();
    let mut any = false;
    loop {
        skip_ows(&mut i);
        if i >= chars.len() {
            break;
        }
        match chars[i] {
            ',' => {
                if any {
                    out.push(std::mem::take(&mut element));
                }
                seen.clear();
                any = false;
                i += 1;
                continue;
            }
            ';' => {
                i += 1;
                continue;
            }
            _ => {}
        }
        // A pair: token "=" (token / quoted-string).
        let start = i;
        while i < chars.len() && tchar(chars[i]) {
            i += 1;
        }
        if i == start {
            return Err(format!("'{}' where a parameter's name belongs", chars[i]));
        }
        let name: String = chars[start..i]
            .iter()
            .collect::<String>()
            .to_ascii_lowercase();
        if i >= chars.len() || chars[i] != '=' {
            return Err(format!("the parameter '{name}' has no '=' and value"));
        }
        i += 1;
        let value = if i < chars.len() && chars[i] == '"' {
            i += 1;
            let mut v = String::new();
            loop {
                match chars.get(i) {
                    None => return Err(format!("the value of '{name}' has no closing quote")),
                    Some('"') => {
                        i += 1;
                        break;
                    }
                    Some('\\') => {
                        let Some(c) = chars.get(i + 1) else {
                            return Err(format!("the value of '{name}' ends in a backslash"));
                        };
                        v.push(*c);
                        i += 2;
                    }
                    Some(c) => {
                        v.push(*c);
                        i += 1;
                    }
                }
            }
            v
        } else {
            let start = i;
            while i < chars.len() && tchar(chars[i]) {
                i += 1;
            }
            if i == start {
                return Err(format!("the parameter '{name}' has no value"));
            }
            chars[start..i].iter().collect()
        };
        if seen.contains(&name) {
            return Err(format!("'{name}' is given twice in one element"));
        }
        seen.push(name.clone());
        any = true;
        match name.as_str() {
            "for" => element.for_ = Some(value),
            "proto" => element.proto = Some(value),
            "host" => element.host = Some(value),
            _ => {}
        }
        skip_ows(&mut i);
        match chars.get(i) {
            None | Some(';') | Some(',') => {}
            Some(c) => return Err(format!("'{c}' after the value of '{name}'")),
        }
    }
    if any {
        out.push(element);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn addr(t: &str) -> Hop {
        Hop::Addr(t.parse().unwrap())
    }

    #[test]
    fn a_hop_is_an_address_with_or_without_a_port() {
        assert_eq!(hop("192.0.2.5"), addr("192.0.2.5:0"));
        assert_eq!(hop(" 192.0.2.5:4711 "), addr("192.0.2.5:4711"));
        assert_eq!(hop("2001:db8::5"), addr("[2001:db8::5]:0"));
        assert_eq!(hop("[2001:db8::5]"), addr("[2001:db8::5]:0"));
        assert_eq!(hop("\"[2001:db8::5]:4711\""), addr("[2001:db8::5]:4711"));
        assert_eq!(hop("192.0.2.5:_port"), addr("192.0.2.5:0"));
        assert_eq!(hop("::ffff:192.0.2.5"), addr("192.0.2.5:0"), "canonical");
        for unknown in [
            "unknown",
            "_hidden",
            "",
            "lmgw.example",
            "192.0.2.5:x",
            "[::1",
        ] {
            assert_eq!(hop(unknown), Hop::Unknown, "{unknown}");
        }
    }

    #[test]
    fn a_list_element_that_is_not_text_spoils_only_itself() {
        let name = HeaderName::from_static("x-forwarded-for");
        let mut h = HeaderMap::new();
        h.append(
            &name,
            HeaderValue::from_bytes(b"\xff\xfe, 203.0.113.5 ,, \xc3\xa9").unwrap(),
        );
        h.append(&name, HeaderValue::from_static("10.89.0.9"));
        let l = list(&h, &name);
        assert_eq!(
            l,
            vec![
                None,
                Some("203.0.113.5".into()),
                Some("\u{e9}".into()),
                Some("10.89.0.9".into()),
            ]
        );
        // As hops: not an address, an address, not an address, an address.
        let hops: Vec<Hop> = l
            .iter()
            .map(|e| e.as_deref().map_or(Hop::Unknown, hop))
            .collect();
        assert_eq!(
            hops,
            vec![
                Hop::Unknown,
                addr("203.0.113.5:0"),
                Hop::Unknown,
                addr("10.89.0.9:0")
            ]
        );
        // A value a proxy sets is refused naming the header.
        let proto = HeaderName::from_static("x-forwarded-proto");
        let mut h = HeaderMap::new();
        h.insert(&proto, HeaderValue::from_bytes(b"https\xff").unwrap());
        let e = single(&h, &proto).unwrap_err();
        assert_eq!(e, "X-Forwarded-Proto carries a value that is not text");
        // Forwarded: obs-text inside a quoted string is no address, not a
        // refusal; outside one it does not parse.
        let mut h = HeaderMap::new();
        h.insert(
            axum::http::header::FORWARDED,
            HeaderValue::from_bytes(b"for=\"\xff\", for=203.0.113.5").unwrap(),
        );
        let e = forwarded(&h).unwrap();
        assert_eq!(hop(e[0].for_.as_deref().unwrap()), Hop::Unknown);
        assert_eq!(e[1].for_.as_deref(), Some("203.0.113.5"));
        h.insert(
            axum::http::header::FORWARDED,
            HeaderValue::from_bytes(b"for=\xff").unwrap(),
        );
        assert!(forwarded(&h)
            .unwrap_err()
            .starts_with("Forwarded does not parse"));
    }

    #[test]
    fn a_set_header_carries_one_value() {
        let name = HeaderName::from_static("x-forwarded-proto");
        let mut h = HeaderMap::new();
        assert_eq!(single(&h, &name).unwrap(), None);
        h.append(&name, HeaderValue::from_static("https"));
        assert_eq!(single(&h, &name).unwrap().as_deref(), Some("https"));
        h.append(&name, HeaderValue::from_static("HTTPS, https"));
        assert_eq!(single(&h, &name).unwrap().as_deref(), Some("https"));
        h.append(&name, HeaderValue::from_static("http"));
        let e = single(&h, &name).unwrap_err();
        assert!(
            e.contains("values that differ (https, HTTPS, https, http)"),
            "{e}"
        );
    }

    #[test]
    fn a_scheme_and_a_host_are_read_strictly() {
        assert_eq!(proto("HTTPS", "p").unwrap(), Scheme::Https);
        assert_eq!(proto("wss", "p").unwrap(), Scheme::Https);
        assert_eq!(proto("ws", "p").unwrap(), Scheme::Http);
        assert!(proto("ftp", "X-Forwarded-Proto")
            .unwrap_err()
            .starts_with("X-Forwarded-Proto names the scheme 'ftp'"));
        assert_eq!(host("lmgw.example:8443", "h").unwrap(), "lmgw.example:8443");
        assert_eq!(
            host("[2001:db8::5]:8443", "h").unwrap(),
            "[2001:db8::5]:8443"
        );
        for bad in [
            "",
            "u@lmgw.example",
            "lmgw.example/x",
            "lmgw .example",
            "lmgw.example:x",
        ] {
            assert!(host(bad, "h").is_err(), "{bad}");
        }
    }

    #[test]
    fn forwarded_reads_its_elements_as_rfc_7239_writes_them() {
        let mut h = HeaderMap::new();
        h.append(
            axum::http::header::FORWARDED,
            HeaderValue::from_static(
                r#"for=192.0.2.60;proto=https;by=203.0.113.43, For="[2001:db8:cafe::17]:4711""#,
            ),
        );
        h.append(
            axum::http::header::FORWARDED,
            HeaderValue::from_static(r#"for=unknown;HOST="lmgw.example:8443";ext=x,,"#),
        );
        let e = forwarded(&h).unwrap();
        assert_eq!(
            e,
            vec![
                Element {
                    for_: Some("192.0.2.60".into()),
                    proto: Some("https".into()),
                    host: None,
                },
                Element {
                    for_: Some("[2001:db8:cafe::17]:4711".into()),
                    ..Default::default()
                },
                Element {
                    for_: Some("unknown".into()),
                    host: Some("lmgw.example:8443".into()),
                    ..Default::default()
                },
            ]
        );
        for bad in [
            "for=192.0.2.60;for=192.0.2.61",
            "for",
            "for=",
            r#"for="192.0.2.60"#,
            "for=192.0.2.60 proto=https",
            "=x",
            "for=[2001:db8::1]",
        ] {
            let mut h = HeaderMap::new();
            h.insert(
                axum::http::header::FORWARDED,
                HeaderValue::from_str(bad).unwrap(),
            );
            let e = forwarded(&h).unwrap_err();
            assert!(e.starts_with("Forwarded does not parse"), "{bad}: {e}");
        }
    }
}
