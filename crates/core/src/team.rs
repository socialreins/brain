//! Team roles and who may change what.

pub const MAX_NAME_CHARS: usize = 60;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Owner,
    Admin,
    Member,
}

impl Role {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "owner" => Some(Self::Owner),
            "admin" => Some(Self::Admin),
            "member" => Some(Self::Member),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Owner => "owner",
            Self::Admin => "admin",
            Self::Member => "member",
        }
    }

    /// Owners and admins manage the team: rename, invite, members.
    pub fn manages(self) -> bool {
        matches!(self, Self::Owner | Self::Admin)
    }

    /// `self` may change another member's role from `from` to `to`, or remove them (`to` is
    /// `None`). Only an owner touches the owner role, on either side.
    pub fn may_change(self, from: Self, to: Option<Self>) -> bool {
        match self {
            Self::Owner => true,
            Self::Admin => from != Self::Owner && to != Some(Self::Owner),
            Self::Member => false,
        }
    }
}

/// The letters of a team code: digits and lowercase letters without the look-alikes `0`, `o`,
/// `1`, `l` and `i`.
pub const CODE_ALPHABET: &[u8; 31] = b"23456789abcdefghjkmnpqrstuvwxyz";
pub const CODE_CHARS: usize = 6;

/// A new random team code. It is an address (`/t/<code>/...`), not a secret, and may clash with
/// an existing one: the `teams.code` unique index decides, and the caller draws again.
pub fn code() -> String {
    let mut out = String::with_capacity(CODE_CHARS);
    while out.len() < CODE_CHARS {
        let mut bytes = [0u8; 16];
        getrandom::fill(&mut bytes).expect("the OS random source is available");
        // 248 is the largest multiple of 31 in a byte; higher bytes would favour some letters.
        let fair = bytes.iter().filter(|b| **b < 248);
        out.extend(fair.map(|b| CODE_ALPHABET[usize::from(b % 31)] as char));
        out.truncate(CODE_CHARS);
    }
    out
}

/// `raw` as a stored team code (lowercase), or `None` when it cannot be one. Codes are matched
/// in any case.
pub fn parse_code(raw: &str) -> Option<String> {
    let code = raw.to_ascii_lowercase();
    (code.len() == CODE_CHARS && code.bytes().all(|b| CODE_ALPHABET.contains(&b))).then_some(code)
}

/// A trimmed team or set name of 1 to `MAX_NAME_CHARS` characters, without control characters
/// (it goes into email subjects).
pub fn name(raw: &str) -> Option<String> {
    name_up_to(raw, MAX_NAME_CHARS)
}

/// `name` with a limit of `max` characters, for other names such as a user's display name.
pub fn name_up_to(raw: &str, max: usize) -> Option<String> {
    let name = raw.trim();
    let ok = !name.is_empty() && name.chars().count() <= max && !name.chars().any(char::is_control);
    ok.then(|| name.to_owned())
}

/// `text` for a plain-text email, unable to pass as a link: every character other than a
/// letter, a digit, a space, `_`, `'` or `-` becomes a space, so no dot of any script, slash or
/// colon survives to form a domain or URL.
pub fn inert(text: &str) -> String {
    text.chars()
        .map(|c| {
            if c.is_alphanumeric() || " _'-".contains(c) {
                c
            } else {
                ' '
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{inert, name, name_up_to, Role::*};

    #[test]
    fn inert_names_cannot_pass_as_links() {
        assert_eq!(
            inert("https://evil.example/login"),
            "https   evil example login"
        );
        assert_eq!(inert("O'Brien-Smith Team_1"), "O'Brien-Smith Team_1");
        assert_eq!(inert("Ünïcode 東京"), "Ünïcode 東京");
        // ASCII, ideographic, fullwidth, halfwidth, one dot leader, small full stop, and others.
        for dot in [
            '.', '\u{3002}', '\u{FF0E}', '\u{FF61}', '\u{2024}', '\u{FE52}', '\u{00B7}',
            '\u{0701}', '\u{2E3C}', '\u{FF1A}', '\u{FF0F}', '@',
        ] {
            assert_eq!(
                inert(&format!("evil{dot}com")),
                "evil com",
                "U+{:04X}",
                dot as u32
            );
        }
    }

    #[test]
    fn only_owners_touch_the_owner_role() {
        assert!(Owner.may_change(Owner, Some(Member)));
        assert!(Owner.may_change(Member, Some(Owner)));
        assert!(Admin.may_change(Member, Some(Admin)));
        assert!(Admin.may_change(Admin, None));
        assert!(!Admin.may_change(Owner, Some(Member)));
        assert!(!Admin.may_change(Member, Some(Owner)));
        assert!(!Admin.may_change(Owner, None));
        assert!(!Member.may_change(Member, None));
        assert!(Owner.manages() && Admin.manages() && !Member.manages());
    }

    #[test]
    fn roles_round_trip() {
        for r in [Owner, Admin, Member] {
            assert_eq!(super::Role::parse(r.as_str()), Some(r));
        }
        assert_eq!(super::Role::parse("Owner"), None);
    }

    #[test]
    fn names_are_trimmed_and_bounded() {
        assert_eq!(name("  Acme  ").as_deref(), Some("Acme"));
        assert_eq!(name("   "), None);
        assert!(name(&"a".repeat(60)).is_some());
        assert_eq!(name(&"a".repeat(61)), None);
        assert_eq!(name("a\r\nBcc: x"), None);
        assert!(name_up_to(&"a".repeat(80), 80).is_some());
        assert_eq!(name_up_to(&"a".repeat(81), 80), None);
    }
}
