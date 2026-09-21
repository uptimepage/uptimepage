//! What a configured sender or destination mailbox may look like. Shared by
//! the boot validator and the destination guard so the two never disagree.

/// The local part and domain of one bare `user@domain`: no display name,
/// brackets, quotes, whitespace or second `@`; a dotted domain with no empty
/// label; at most 64 bytes before the `@` and 254 in all. Narrower than RFC
/// 5322 on purpose: quoted local parts and comments are legal and nobody
/// configures them.
pub fn parse_bare(address: &str) -> Option<(&str, &str)> {
    let (local, domain) = address.split_once('@')?;
    let local_ok = !local.is_empty()
        && local.len() <= 64
        && local
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "!#$%&'*+/=?^_`{|}~.-".contains(c));
    let domain_ok = domain.contains('.')
        && !domain.starts_with('.')
        && !domain.ends_with('.')
        && !domain.contains("..")
        && domain
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.');
    (local_ok && domain_ok && address.len() <= 254).then_some((local, domain))
}

/// `no-reply`, `noreply+alerts`, `alerts-no-reply`, `do_not_reply`,
/// `no-replies` and the like: some run of whole words spells it, so
/// `juno.reply` is not one.
pub fn is_no_reply(local: &str) -> bool {
    let local = local.to_ascii_lowercase();
    let base = local
        .split_once('+')
        .map_or(local.as_str(), |(base, _)| base);
    let words: Vec<&str> = base
        .split(['-', '.', '_'])
        .map(|w| w.trim_end_matches(|c: char| c.is_ascii_digit()))
        .collect();
    (0..words.len()).any(|i| {
        (i + 1..=words.len()).any(|j| {
            matches!(
                words[i..j].concat().as_str(),
                "noreply" | "noreplies" | "donotreply" | "donotreplies"
            )
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bare_address_is_one_plain_user_at_dotted_domain() {
        assert_eq!(
            parse_bare("status+alerts@acme.test"),
            Some(("status+alerts", "acme.test"))
        );
        for bad in [
            "",
            "hello",
            "@example.test",
            "hello@",
            "a@b",
            "alerts@localhost",
            "hello@example.test.",
            "hello@.example.test",
            "hello@acme..test",
            "Acme <hello@example.test>",
            "<hello@example.test>",
            "\"Acme\"<hello@example.test>",
            " hello@example.test",
            "hello@example.test\n",
            "hello@acme@example.test",
            "hello,ops@example.test",
        ] {
            assert!(parse_bare(bad).is_none(), "{bad:?}");
        }
        assert!(parse_bare(&format!("{}@example.test", "a".repeat(65))).is_none());
        assert!(parse_bare(&format!("a@{}.test", "b".repeat(250))).is_none());
    }

    #[test]
    fn no_reply_is_spelled_many_ways() {
        for local in [
            "no-reply",
            "noreply",
            "No.Reply",
            "do-not-reply",
            "do_not_reply",
            "noreply+alerts",
            "alerts-noreply",
            "noreply2",
            "no-reply-alerts",
            "alerts-no-reply",
            "status.no.reply",
            "do-not-reply-alerts",
            "noreplies",
            "no-replies",
            "do-not-replies",
        ] {
            assert!(is_no_reply(local), "{local}");
        }
        for local in [
            "hello",
            "replies",
            "juno.reply",
            "technoreply",
            "reply-no",
            "",
        ] {
            assert!(!is_no_reply(local), "{local}");
        }
    }
}
