//! Encryption at rest for platform secrets (access tokens, Bluesky app passwords): AES-256-GCM
//! with a random 96-bit nonce per value. A sealed value is `key id (1 byte) || nonce || ciphertext
//! and tag`; the key id is also the associated data, so a later key can be added for rotation.
//! shortcut: random 96-bit nonces stay safe for about 2^32 values per key; rotate before that.
use base64::{engine::general_purpose::STANDARD, Engine};
use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, AES_256_GCM, NONCE_LEN};

/// The id of the one key in use today (`ENCRYPTION_KEY`).
const KEY_ID: u8 = 1;

/// No `Debug`: it holds the key.
pub struct Cipher(LessSafeKey);

impl Cipher {
    /// 32 bytes, standard base64 (`openssl rand -base64 32`). `None` for anything else.
    pub fn from_base64(key: &str) -> Option<Self> {
        let bytes = STANDARD.decode(key.trim()).ok()?;
        // `UnboundKey::new` refuses any length but 32.
        let key = UnboundKey::new(&AES_256_GCM, &bytes).ok()?;
        Some(Self(LessSafeKey::new(key)))
    }

    pub fn encrypt(&self, plain: &[u8]) -> Vec<u8> {
        let mut nonce = [0u8; NONCE_LEN];
        getrandom::fill(&mut nonce).expect("the OS random source is available");
        let mut sealed = plain.to_vec();
        self.0
            .seal_in_place_append_tag(
                Nonce::assume_unique_for_key(nonce),
                Aad::from([KEY_ID]),
                &mut sealed,
            )
            .expect("AES-GCM seals any input under 64 GiB");
        [&[KEY_ID][..], &nonce, &sealed].concat()
    }

    /// `None` when the value was sealed by another key, cut short or tampered with.
    pub fn decrypt(&self, sealed: &[u8]) -> Option<Vec<u8>> {
        let (&id, rest) = sealed.split_first()?;
        if id != KEY_ID || rest.len() < NONCE_LEN {
            return None;
        }
        let (nonce, body) = rest.split_at(NONCE_LEN);
        let nonce = Nonce::try_assume_unique_for_key(nonce).ok()?;
        let mut body = body.to_vec();
        let plain = self
            .0
            .open_in_place(nonce, Aad::from([id]), &mut body)
            .ok()?;
        Some(plain.to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::Cipher;

    const KEY: &str = "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=";

    #[test]
    fn round_trips_with_a_fresh_nonce_each_time() {
        let c = Cipher::from_base64(KEY).unwrap();
        let (a, b) = (c.encrypt(b"app-pass"), c.encrypt(b"app-pass"));
        assert_ne!(a, b);
        assert_eq!(a[0], 1, "key id leads");
        assert_eq!(a.len(), 1 + 12 + 8 + 16);
        assert!(!a.windows(8).any(|w| w == b"app-pass"));
        assert_eq!(c.decrypt(&a).unwrap(), b"app-pass");
        assert_eq!(c.decrypt(&b).unwrap(), b"app-pass");
    }

    #[test]
    fn refuses_tampering_other_keys_and_short_input() {
        let c = Cipher::from_base64(KEY).unwrap();
        let sealed = c.encrypt(b"secret");
        for i in 0..sealed.len() {
            let mut bad = sealed.clone();
            bad[i] ^= 1;
            assert!(c.decrypt(&bad).is_none(), "byte {i}");
        }
        let other = Cipher::from_base64("HwAeAB0AHAAbABoAGQAYABcAFgAVABQAEwASABEAEAA=").unwrap();
        assert!(other.decrypt(&sealed).is_none());
        assert!(c.decrypt(&[]).is_none());
        assert!(c.decrypt(&sealed[..10]).is_none());
    }

    #[test]
    fn key_must_be_32_bytes_of_base64() {
        assert!(Cipher::from_base64(&format!(" {KEY}\n")).is_some());
        assert!(Cipher::from_base64("AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHg==").is_none());
        assert!(Cipher::from_base64("not base64!").is_none());
        assert!(Cipher::from_base64("").is_none());
    }
}
