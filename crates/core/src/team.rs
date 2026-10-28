//! Team roles and who may change what.

pub const MAX_NAME_CHARS: usize = 100;

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

/// A trimmed team or set name of 1 to `MAX_NAME_CHARS` characters, without control characters
/// (it goes into email subjects).
pub fn name(raw: &str) -> Option<String> {
    let name = raw.trim();
    let ok = !name.is_empty()
        && name.chars().count() <= MAX_NAME_CHARS
        && !name.chars().any(char::is_control);
    ok.then(|| name.to_owned())
}

#[cfg(test)]
mod tests {
    use super::{name, Role::*};

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
        assert!(name(&"a".repeat(100)).is_some());
        assert_eq!(name(&"a".repeat(101)), None);
        assert_eq!(name("a\r\nBcc: x"), None);
    }
}
