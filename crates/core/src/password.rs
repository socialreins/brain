//! Password rules and Argon2id hashing.
use argon2::{
    password_hash::{PasswordHash, SaltString},
    Argon2, PasswordHasher, PasswordVerifier,
};

pub const MIN_CHARS: usize = 12;
pub const MAX_CHARS: usize = 256;

/// Long enough and not absurdly long (the hash runs on every login attempt).
pub fn acceptable(password: &str) -> bool {
    (MIN_CHARS..=MAX_CHARS).contains(&password.chars().count())
}

/// Argon2id PHC string with a random salt, using the crate's default cost.
pub fn hash(password: &str) -> String {
    let mut salt = [0u8; 16];
    getrandom::fill(&mut salt).expect("the OS random source is available");
    let salt = SaltString::encode_b64(&salt).expect("16 bytes is a valid salt");
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .expect("argon2 accepts any password up to MAX_CHARS")
        .to_string()
}

/// True when `password` matches `phc`; false for a mismatch or a malformed hash.
pub fn verify(password: &str, phc: &str) -> bool {
    PasswordHash::new(phc).is_ok_and(|h| {
        Argon2::default()
            .verify_password(password.as_bytes(), &h)
            .is_ok()
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn hashes_verify_and_salt() {
        let h = super::hash("correct horse battery");
        assert!(h.starts_with("$argon2id$"));
        assert!(super::verify("correct horse battery", &h));
        assert!(!super::verify("wrong horse battery", &h));
        assert!(!super::verify("x", "not a hash"));
        assert_ne!(h, super::hash("correct horse battery"));
    }

    #[test]
    fn length_counts_characters() {
        assert!(!super::acceptable(&"a".repeat(11)));
        assert!(super::acceptable(&"é".repeat(12)));
        assert!(super::acceptable(&"a".repeat(256)));
        assert!(!super::acceptable(&"a".repeat(257)));
    }
}
