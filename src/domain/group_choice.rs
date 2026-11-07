use uuid::Uuid;

/// A group name as every write stores it: `str::trim`, the policy the
/// database's `group_name_trim` mirrors, and blank meaning no group.
pub fn normalize_group_name(name: &str) -> Option<&str> {
    Some(name.trim()).filter(|n| !n.is_empty())
}

/// The group a monitor write joins: by name, created when new, or by id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupChoice<'a> {
    Name(&'a str),
    Id(Uuid),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_is_trimmed_and_blank_is_no_group() {
        assert_eq!(normalize_group_name("  DNS\u{2003}"), Some("DNS"));
        assert_eq!(normalize_group_name("\u{00A0}\u{3000} "), None);
        assert_eq!(normalize_group_name(""), None);
        assert_eq!(normalize_group_name("\u{200B}"), Some("\u{200B}"));
    }
}
