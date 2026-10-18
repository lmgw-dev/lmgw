//! The CA key at rest (device link design D2): wrapped, the PKCS#8 key
//! encrypted with AES-256-GCM under a random 256-bit key that the session's
//! Secret Service holds; plain only on a start without one.
//!
//! The key file is PEM either way, and its label says which:
//! [`PLAIN_LABEL`] is the standard PKCS#8 one (`openssl pkey` reads it), and
//! [`WRAPPED_LABEL`] carries a version byte, the 96-bit nonce and the
//! ciphertext with its tag, sealed under [`AAD`]. Each wrapping key seals
//! exactly one key file: a new one is made whenever the file is written
//! wrapped, so a nonce drawn at random never repeats under a key.

use secrecy::zeroize::Zeroizing;
use secrecy::{ExposeSecret, SecretBox};

/// The label of a wrapped key file.
pub const WRAPPED_LABEL: &str = "LMGW WRAPPED PRIVATE KEY";
/// The label of a plain key file: PKCS#8, as RFC 7468 names it.
pub const PLAIN_LABEL: &str = "PRIVATE KEY";
/// What the envelope's tag also covers: the purpose and the format, so a
/// wrapping key's ciphertext opens as nothing else.
pub const AAD: &[u8] = b"lmgw device CA key, wrapped, v1";
/// The envelope's format.
const VERSION: u8 = 1;
const NONCE_LEN: usize = 12;

/// A 256-bit wrapping key. Its bytes are wiped when it drops, and neither
/// `Debug` nor any log shows them.
pub struct WrapKey(SecretBox<[u8; 32]>);

impl std::fmt::Debug for WrapKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("WrapKey([redacted])")
    }
}

impl WrapKey {
    /// A fresh key from the OS's random source.
    pub fn generate() -> Result<Self, String> {
        let mut bytes = Box::new([0u8; 32]);
        aws_lc_rs::rand::fill(bytes.as_mut())
            .map_err(|_| "the random source failed to make a wrapping key".to_string())?;
        Ok(Self(SecretBox::new(bytes)))
    }

    /// Whether `other` is the same key, compared in constant time.
    pub fn same(&self, other: &WrapKey) -> bool {
        aws_lc_rs::constant_time::verify_slices_are_equal(
            self.0.expose_secret(),
            other.0.expose_secret(),
        )
        .is_ok()
    }

    /// The key as a store keeps it: 64 lowercase hex digits.
    pub fn to_hex(&self) -> Zeroizing<String> {
        Zeroizing::new(hex::encode(self.0.expose_secret()))
    }

    /// A key a store gave back as [`Self::to_hex`] wrote it; `None` for
    /// anything else.
    pub fn from_hex(text: &[u8]) -> Option<Self> {
        let text = std::str::from_utf8(text).ok()?.trim();
        let mut bytes = Box::new([0u8; 32]);
        hex::decode_to_slice(text, bytes.as_mut()).ok()?;
        Some(Self(SecretBox::new(bytes)))
    }

    fn key(&self) -> Result<aws_lc_rs::aead::RandomizedNonceKey, String> {
        aws_lc_rs::aead::RandomizedNonceKey::new(
            &aws_lc_rs::aead::AES_256_GCM,
            self.0.expose_secret(),
        )
        .map_err(|_| "the wrapping key cannot be used for AES-256-GCM".to_string())
    }
}

/// The envelope for `pkcs8` under `key`: version, nonce, ciphertext and
/// tag.
pub fn seal(key: &WrapKey, pkcs8: &[u8]) -> Result<Vec<u8>, String> {
    let mut body = Zeroizing::new(pkcs8.to_vec());
    let nonce = key
        .key()?
        .seal_in_place_append_tag(aws_lc_rs::aead::Aad::from(AAD), &mut *body)
        .map_err(|_| "sealing the CA key failed".to_string())?;
    let mut out = Vec::with_capacity(1 + NONCE_LEN + body.len());
    out.push(VERSION);
    out.extend_from_slice(nonce.as_ref());
    out.extend_from_slice(&body);
    Ok(out)
}

/// The PKCS#8 key inside `envelope`, if `key` opens it.
pub fn open(key: &WrapKey, envelope: &[u8]) -> Result<Zeroizing<Vec<u8>>, String> {
    let (&version, rest) = envelope
        .split_first()
        .ok_or_else(|| "the wrapped key is empty".to_string())?;
    if version != VERSION {
        return Err(format!(
            "the wrapped key has format {version}; this lmgw reads format {VERSION}"
        ));
    }
    if rest.len() < NONCE_LEN {
        return Err("the wrapped key is cut short".into());
    }
    let (nonce, sealed) = rest.split_at(NONCE_LEN);
    let nonce = aws_lc_rs::aead::Nonce::try_assume_unique_for_key(nonce)
        .map_err(|_| "the wrapped key's nonce is malformed".to_string())?;
    let mut body = Zeroizing::new(sealed.to_vec());
    let plain = key
        .key()?
        .open_in_place(nonce, aws_lc_rs::aead::Aad::from(AAD), &mut body)
        .map_err(|_| "the wrapping key does not open it".to_string())?;
    let len = plain.len();
    body.truncate(len);
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sealed_key_opens_with_its_key_only() {
        let k = WrapKey::generate().unwrap();
        let other = WrapKey::generate().unwrap();
        let sealed = seal(&k, b"pkcs8 bytes").unwrap();
        assert_eq!(&*open(&k, &sealed).unwrap(), b"pkcs8 bytes");
        assert_eq!(
            open(&other, &sealed).unwrap_err(),
            "the wrapping key does not open it"
        );
        let mut flipped = sealed.clone();
        *flipped.last_mut().unwrap() ^= 1;
        assert!(open(&k, &flipped).is_err());
        let mut v2 = sealed;
        v2[0] = 2;
        assert!(open(&k, &v2).unwrap_err().contains("format 2"));
    }

    #[test]
    fn a_key_round_trips_as_hex_and_never_shows() {
        let k = WrapKey::generate().unwrap();
        let back = WrapKey::from_hex(k.to_hex().as_bytes()).unwrap();
        let sealed = seal(&k, b"x").unwrap();
        assert_eq!(&*open(&back, &sealed).unwrap(), b"x");
        assert_eq!(format!("{k:?}"), "WrapKey([redacted])");
        assert!(WrapKey::from_hex(b"abcd").is_none());
        assert!(WrapKey::from_hex(&[0xff; 64]).is_none());
    }
}
