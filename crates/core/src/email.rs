//! Email address checks for signups.

/// Trims and lowercases `raw`, returning it when it looks like a deliverable address.
/// shortcut: syntax check only (no MX lookup, no quoted local parts), upgrade if bounces show up.
pub fn normalize(raw: &str) -> Option<String> {
    let email = raw.trim().to_lowercase();
    let (local, domain) = email.split_once('@')?;
    let labels_ok = domain.split('.').all(|l| {
        !l.is_empty()
            && !l.starts_with('-')
            && !l.ends_with('-')
            && l.chars().all(|c| c.is_alphanumeric() || c == '-')
    });
    let ok = email.len() <= 254
        && !local.is_empty()
        && local.len() <= 64
        && !local.starts_with('.')
        && !local.ends_with('.')
        && !local.contains("..")
        && local
            .chars()
            .all(|c| c.is_alphanumeric() || "!#$%&'*+/=?^_`{|}~.-".contains(c))
        && domain.contains('.')
        && labels_ok;
    ok.then_some(email)
}

#[cfg(test)]
mod tests {
    use super::normalize;

    #[test]
    fn accepts_and_lowercases() {
        assert_eq!(
            normalize("  Jane.Doe+x@Example.COM ").as_deref(),
            Some("jane.doe+x@example.com")
        );
    }

    #[test]
    fn rejects_malformed() {
        for bad in [
            "",
            "plain",
            "@x.com",
            "a@",
            "a@b",
            "a@@b.com",
            "a b@c.com",
            "a@b..com",
            ".a@b.com",
            "a@-b.com",
        ] {
            assert_eq!(normalize(bad), None, "{bad}");
        }
    }
}
