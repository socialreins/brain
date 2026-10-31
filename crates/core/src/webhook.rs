//! Outbound webhook signatures: `t=<unix seconds>,v1=<hex HMAC-SHA256(secret, "<t>.<body>")>`,
//! keyed by the whole secret string, `whsec_` included.
use ring::hmac;

/// The `SocialReins-Signature` header value for `body` sent at unix second `t`.
pub fn signature(secret: &str, t: i64, body: &str) -> String {
    let key = hmac::Key::new(hmac::HMAC_SHA256, secret.as_bytes());
    let tag = hmac::sign(&key, format!("{t}.{body}").as_bytes());
    format!("t={t},v1={}", hex::encode(tag.as_ref()))
}

#[cfg(test)]
mod tests {
    #[test]
    fn signature_matches_a_known_vector() {
        // Checked with: printf '1700000000.{"a":1}' | openssl dgst -sha256 -hmac whsec_test
        assert_eq!(
            super::signature("whsec_test", 1_700_000_000, r#"{"a":1}"#),
            "t=1700000000,v1=38877139021993b830af32feea6e18a8da83eb2f6e49ee50bd9e4cf4ca4d3789"
        );
    }
}
