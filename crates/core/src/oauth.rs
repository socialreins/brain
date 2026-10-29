//! Pieces of an OAuth sign-in that need no network: PKCE and the `next` path.
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use sha2::{Digest, Sha256};

/// The S256 PKCE challenge for `verifier` (RFC 7636).
pub fn pkce_challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

/// `next` when it is a path on this site: starts with one `/`, no `//` or `\` (which browsers
/// read as another host), no control characters, at most 512 bytes. Anything else is `None`.
pub fn safe_next(next: &str) -> Option<&str> {
    let ok = next.starts_with('/')
        && !next.starts_with("//")
        && !next.contains('\\')
        && next.len() <= 512
        && !next.chars().any(|c| c.is_control() || c.is_whitespace());
    ok.then_some(next)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_matches_the_rfc_example() {
        assert_eq!(
            pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn next_stays_on_this_site() {
        for ok in ["/", "/app", "/app/team?tab=members#x", "/invitations/abc"] {
            assert_eq!(safe_next(ok), Some(ok), "{ok}");
        }
        for bad in [
            "",
            "app",
            "//evil.example",
            "/\\evil.example",
            "https://evil.example",
            "/a b",
            "/a\nb",
            "/a\tb",
            "javascript:alert(1)",
        ] {
            assert_eq!(safe_next(bad), None, "{bad:?}");
        }
        assert_eq!(safe_next(&format!("/{}", "a".repeat(512))), None);
    }
}
