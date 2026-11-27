//! Email address checks for signups.

/// Trims and lowercases `raw`, returning it when it looks like a deliverable address.
/// The domain is ASCII letters, digits, hyphen and dot: a name that is not stays away, its
/// `xn--` form is what an international one looks like on the wire.
/// shortcut: syntax check only (no MX lookup, no quoted local parts), upgrade if bounces show up.
pub fn normalize(raw: &str) -> Option<String> {
    let email = raw.trim().to_lowercase();
    let (local, domain) = email.split_once('@')?;
    let labels_ok = domain.split('.').all(|l| {
        !l.is_empty()
            && !l.starts_with('-')
            && !l.ends_with('-')
            && l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
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

/// The person behind an address, for things one person gets once (the free trial): the
/// normalized address without a `+tag`, and for Gmail also without dots. `None` for a bad address.
/// shortcut: other providers' aliases and a person with several real mailboxes pass this; billing
/// also checks the card fingerprint where checkout collected one (`trial_uses.card_hash`).
pub fn person_key(raw: &str) -> Option<String> {
    let email = normalize(raw)?;
    let (local, domain) = email.split_once('@')?;
    let local = local.split('+').next().unwrap_or(local);
    if local.is_empty() {
        return None;
    }
    Some(match domain {
        "gmail.com" | "googlemail.com" => format!("{}@gmail.com", local.replace('.', "")),
        _ => format!("{local}@{domain}"),
    })
}

#[cfg(test)]
mod tests {
    use super::{normalize, person_key};

    #[test]
    fn one_person_one_key() {
        for same in [
            "Jane.Doe+a@gmail.com",
            "janedoe@googlemail.com",
            "j.a.n.e.d.o.e+x+y@gmail.com",
        ] {
            assert_eq!(
                person_key(same).as_deref(),
                Some("janedoe@gmail.com"),
                "{same}"
            );
        }
        assert_eq!(
            person_key("a.b+c@corp.example").as_deref(),
            Some("a.b@corp.example")
        );
        assert_ne!(person_key("a@corp.example"), person_key("a@other.example"));
        assert_eq!(person_key("nope"), None);
        // Nothing before the +tag: no person to name.
        assert_eq!(person_key("+a@corp.example"), None);
    }

    #[test]
    fn accepts_and_lowercases() {
        assert_eq!(
            normalize("  Jane.Doe+x@Example.COM ").as_deref(),
            Some("jane.doe+x@example.com")
        );
    }

    #[test]
    fn the_domain_is_ascii_only_and_punycode_passes() {
        // An internationalised domain as typed is refused; its `xn--` form is ASCII and is what
        // SMTP carries, so it is accepted.
        for bad in ["user@почта.рф", "user@bücher.de", "user@例え.テスト"] {
            assert_eq!(normalize(bad), None, "{bad}");
        }
        assert_eq!(
            normalize("  User@XN--BCHER-KVA.de ").as_deref(),
            Some("user@xn--bcher-kva.de")
        );
    }

    #[test]
    fn the_local_part_keeps_the_quoting_characters() {
        // RFC 5321 atext, which SMTP delivers: a local part may hold them. Only the domain is
        // held to ASCII.
        for ok in [
            "o'brien@example.com",
            "a|b@example.com",
            "a`b@example.com",
            "a{b}@example.com",
        ] {
            assert_eq!(normalize(ok).as_deref(), Some(ok), "{ok}");
        }
        assert_eq!(normalize("o'brien@exämple.com"), None);
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
