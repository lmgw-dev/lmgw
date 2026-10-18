//! Just enough DER to find a certificate's SubjectPublicKeyInfo (RFC 5280
//! §4.1), with no crypto provider: the enrolment's reader checks `fp` and
//! the issued certificate's key where the `tls` feature is off (device link
//! design §6.3), and the `tls` verifiers find the pinned CA by the same
//! bytes.

const INTEGER: u8 = 0x02;
const SEQUENCE: u8 = 0x30;
/// `version [0] EXPLICIT`, absent in a v1 certificate.
const VERSION: u8 = 0xa0;

/// One DER element at the head of an input.
struct Element<'a> {
    tag: u8,
    /// The whole element, its tag and length included.
    whole: &'a [u8],
    value: &'a [u8],
    /// What follows it.
    rest: &'a [u8],
}

/// The DER element at the head of `input`. `None` for anything DER does
/// not allow here: a multi-byte tag (a certificate's head has none), an
/// indefinite or non-minimal length, a length past the input, or one over
/// four bytes (an element over 4 GiB, which no certificate is).
fn element(input: &[u8]) -> Option<Element<'_>> {
    let (&tag, rest) = input.split_first()?;
    if tag & 0x1f == 0x1f {
        return None;
    }
    let (&first, rest) = rest.split_first()?;
    let (len, rest) = if first < 0x80 {
        (usize::from(first), rest)
    } else {
        let n = usize::from(first & 0x7f);
        if n == 0 || n > 4 || rest.len() < n || rest[0] == 0 {
            return None;
        }
        let len = rest[..n]
            .iter()
            .fold(0usize, |len, &b| (len << 8) | usize::from(b));
        if len < 0x80 {
            return None;
        }
        (len, &rest[n..])
    };
    if rest.len() < len {
        return None;
    }
    let header = input.len() - rest.len();
    Some(Element {
        tag,
        whole: &input[..header + len],
        value: &rest[..len],
        rest: &rest[len..],
    })
}

/// The DER SubjectPublicKeyInfo of DER certificate `cert`, its tag and
/// length included: the bytes `fp`, a device row's key hash and the
/// confirmation code are taken over. `None` when `cert` is no certificate
/// (trailing bytes included).
pub(crate) fn certificate_spki(cert: &[u8]) -> Option<&[u8]> {
    let certificate = element(cert).filter(|e| e.tag == SEQUENCE && e.rest.is_empty())?;
    let tbs = element(certificate.value).filter(|e| e.tag == SEQUENCE)?;
    let mut fields = tbs.value;
    if let Some(version) = element(fields).filter(|e| e.tag == VERSION) {
        fields = version.rest;
    }
    // serialNumber, signature, issuer, validity, subject.
    for want in [INTEGER, SEQUENCE, SEQUENCE, SEQUENCE, SEQUENCE] {
        fields = element(fields).filter(|e| e.tag == want)?.rest;
    }
    let spki = element(fields).filter(|e| e.tag == SEQUENCE)?;
    Some(spki.whole)
}

#[cfg(test)]
mod tests {
    use rcgen::{CertificateParams, KeyPair, PublicKeyData};

    use super::*;

    #[test]
    fn a_certificate_s_key_is_found_as_rcgen_wrote_it() {
        let key = KeyPair::generate().unwrap();
        let cert = CertificateParams::new(vec!["lmgw.test".to_string()])
            .unwrap()
            .self_signed(&key)
            .unwrap();
        let spki = key.subject_public_key_info();
        assert_eq!(certificate_spki(cert.der()), Some(spki.as_slice()));
        // A truncated or extended certificate, a bare key and nothing are
        // no certificate.
        let der = cert.der().to_vec();
        assert_eq!(certificate_spki(&der[..der.len() - 1]), None);
        assert_eq!(certificate_spki(&[der.as_slice(), &[0]].concat()), None);
        assert_eq!(certificate_spki(&spki), None);
        assert_eq!(certificate_spki(&[]), None);
    }

    #[test]
    fn lengths_are_strict_der() {
        assert_eq!(
            element(&[0x04, 0x01, 0xaa]).map(|e| e.value),
            Some(&[0xaa][..])
        );
        // Indefinite, non-minimal, past the input, over four bytes.
        assert!(element(&[0x30, 0x80, 0, 0]).is_none());
        assert!(element(&[0x04, 0x81, 0x01, 0xaa]).is_none());
        assert!(element(&[0x04, 0x82, 0x00, 0x90]).is_none());
        assert!(element(&[0x04, 0x02, 0xaa]).is_none());
        assert!(element(&[0x04, 0x85, 1, 0, 0, 0, 0]).is_none());
        // A long form is read when it is due.
        let long = [&[0x04, 0x81, 0x80][..], &[7; 0x80]].concat();
        assert_eq!(element(&long).map(|e| e.value.len()), Some(0x80));
    }
}
