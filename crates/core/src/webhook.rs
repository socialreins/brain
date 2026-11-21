//! Outbound webhook signatures: `t=<unix seconds>,v1=<hex HMAC-SHA256(secret, "<t>.<body>")>`,
//! keyed by the whole secret string, `whsec_` included.
use ring::hmac;

/// The `SocialReins-Signature` header value for `body` sent at unix second `t`.
pub fn signature(secret: &str, t: i64, body: &str) -> String {
    let key = hmac::Key::new(hmac::HMAC_SHA256, secret.as_bytes());
    let tag = hmac::sign(&key, format!("{t}.{body}").as_bytes());
    format!("t={t},v1={}", hex::encode(tag.as_ref()))
}

/// The `X-Hub-Signature-256` value Meta sends for `body`: `sha256=<hex HMAC-SHA256>`. Brain only
/// verifies these; tests sign with it.
pub fn sign_sha256(secret: &str, body: &[u8]) -> String {
    let key = hmac::Key::new(hmac::HMAC_SHA256, secret.as_bytes());
    format!("sha256={}", hex::encode(hmac::sign(&key, body).as_ref()))
}

/// Whether `header` is `sha256=<hex HMAC-SHA256(secret, body)>`, the signature Meta puts in
/// `X-Hub-Signature-256` on its webhook posts. The comparison is constant time.
pub fn verify_sha256(secret: &str, body: &[u8], header: &str) -> bool {
    let Some(tag) = header
        .strip_prefix("sha256=")
        .and_then(|h| hex::decode(h).ok())
    else {
        return false;
    };
    let key = hmac::Key::new(hmac::HMAC_SHA256, secret.as_bytes());
    hmac::verify(&key, body, &tag).is_ok()
}

/// Whether `header` is a `t=<unix seconds>,v1=<hex>[,v1=<hex>...]` signature (the format of
/// `signature`, and the one Stripe sends) of `body` under `secret`: some `v1` must be the HMAC of
/// `<t>.<body>`, compared in constant time, and `t` within `tolerance` seconds of `now`.
pub fn verify_timestamped(
    secret: &str,
    body: &[u8],
    header: &str,
    now: i64,
    tolerance: i64,
) -> bool {
    let mut t = None;
    let mut tags: Vec<Vec<u8>> = Vec::new();
    for part in header.split(',') {
        match part.trim().split_once('=') {
            Some(("t", v)) => t = v.parse::<i64>().ok(),
            Some(("v1", v)) => tags.extend(hex::decode(v)),
            _ => {}
        }
    }
    let Some(t) = t.filter(|t| now.abs_diff(*t) <= tolerance.unsigned_abs()) else {
        return false;
    };
    let key = hmac::Key::new(hmac::HMAC_SHA256, secret.as_bytes());
    let mut signed = format!("{t}.").into_bytes();
    signed.extend_from_slice(body);
    tags.iter()
        .any(|tag| hmac::verify(&key, &signed, tag).is_ok())
}

#[cfg(test)]
mod tests {
    #[test]
    fn timestamped_signatures_need_a_fresh_t_and_one_good_v1() {
        let sig = super::signature("whsec_x", 1000, "{}");
        let (now, tol) = (1100, 300);
        assert!(super::verify_timestamped("whsec_x", b"{}", &sig, now, tol));
        assert!(!super::verify_timestamped("whsec_y", b"{}", &sig, now, tol));
        assert!(!super::verify_timestamped(
            "whsec_x", b"{ }", &sig, now, tol
        ));
        assert!(!super::verify_timestamped(
            "whsec_x", b"{}", &sig, 1301, tol
        ));
        assert!(!super::verify_timestamped("whsec_x", b"{}", &sig, 699, tol));
        // A rotated secret sends two v1 values; either may be the good one.
        let v1 = sig.split_once("v1=").unwrap().1;
        let two = format!("t=1000,v1={},v1={v1}", "00".repeat(32));
        assert!(super::verify_timestamped("whsec_x", b"{}", &two, now, tol));
        assert!(!super::verify_timestamped(
            "whsec_x", b"{}", "t=1000", now, tol
        ));
        assert!(!super::verify_timestamped(
            "whsec_x", b"{}", "v1=zz", now, tol
        ));
    }

    #[test]
    fn meta_signatures_are_checked_whole() {
        // Checked with: printf '{"a":1}' | openssl dgst -sha256 -hmac s3cret
        let good = "sha256=5910e62016ef5034272c926c27071992a465c2335cecf41851bda071577f4f6d";
        assert_eq!(super::sign_sha256("s3cret", br#"{"a":1}"#), good);
        assert!(super::verify_sha256("s3cret", br#"{"a":1}"#, good));
        assert!(!super::verify_sha256("s3cret", br#"{"a":2}"#, good));
        assert!(!super::verify_sha256("other", br#"{"a":1}"#, good));
        for bad in ["", "sha256=", "sha256=zz", "5910e620", "sha1=5910e620"] {
            assert!(!super::verify_sha256("s3cret", br#"{"a":1}"#, bad));
        }
        assert!(!super::verify_sha256(
            "s3cret",
            br#"{"a":1}"#,
            &good[..good.len() - 2]
        ));
    }

    #[test]
    fn signature_matches_a_known_vector() {
        // Checked with: printf '1700000000.{"a":1}' | openssl dgst -sha256 -hmac whsec_test
        assert_eq!(
            super::signature("whsec_test", 1_700_000_000, r#"{"a":1}"#),
            "t=1700000000,v1=38877139021993b830af32feea6e18a8da83eb2f6e49ee50bd9e4cf4ca4d3789"
        );
    }
}
