//! A published iCalendar (RFC 5545) of timed events, the shape a calendar app
//! subscribes to.

use chrono::{DateTime, Utc};

/// How often a subscribed app should fetch the feed again.
const REFRESH: &str = "PT1H";

/// Content lines longer than this many octets fold onto the next line.
const LINE_OCTETS: usize = 75;

/// A timed event, shown as free time rather than busy.
pub struct Event {
    /// Unique and stable across fetches, so an app updates rather than adds.
    pub uid: String,
    pub starts_at: DateTime<Utc>,
    pub ends_at: DateTime<Utc>,
    pub summary: String,
}

/// The calendar `name` holding `events`, as of `stamp`.
pub fn calendar(name: &str, stamp: DateTime<Utc>, events: &[Event]) -> String {
    let mut out = String::new();
    let mut line = |l: &str| push_line(&mut out, l);
    line("BEGIN:VCALENDAR");
    line("VERSION:2.0");
    line("PRODID:-//Uptimepage//On-call//EN");
    line("CALSCALE:GREGORIAN");
    line("METHOD:PUBLISH");
    line(&format!("X-WR-CALNAME:{}", text(name)));
    line(&format!("REFRESH-INTERVAL;VALUE=DURATION:{REFRESH}"));
    line(&format!("X-PUBLISHED-TTL:{REFRESH}"));
    for e in events {
        line("BEGIN:VEVENT");
        line(&format!("UID:{}", text(&e.uid)));
        line(&format!("DTSTAMP:{}", instant(stamp)));
        line(&format!("DTSTART:{}", instant(e.starts_at)));
        line(&format!("DTEND:{}", instant(e.ends_at)));
        line(&format!("SUMMARY:{}", text(&e.summary)));
        line("TRANSP:TRANSPARENT");
        line("END:VEVENT");
    }
    line("END:VCALENDAR");
    out
}

fn instant(at: DateTime<Utc>) -> String {
    at.format("%Y%m%dT%H%M%SZ").to_string()
}

/// A TEXT value: its separators escaped, line breaks kept as `\n`, other
/// control characters dropped.
fn text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' | ';' | ',' => {
                out.push('\\');
                out.push(c);
            }
            '\n' => out.push_str("\\n"),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

/// One content line ended by CRLF, folded so no line runs past
/// [`LINE_OCTETS`] and no character is split.
fn push_line(out: &mut String, line: &str) {
    let mut width = 0;
    for c in line.chars() {
        if width + c.len_utf8() > LINE_OCTETS {
            out.push_str("\r\n ");
            width = 1;
        }
        out.push(c);
        width += c.len_utf8();
    }
    out.push_str("\r\n");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    #[test]
    fn events_carry_utc_times_and_escaped_text() {
        let ics = calendar(
            "On call · Acme, Inc",
            t("2026-09-26T10:00:00Z"),
            &[Event {
                uid: "a-1@uptimepage".into(),
                starts_at: t("2026-09-28T06:00:00Z"),
                ends_at: t("2026-10-05T06:00:00Z"),
                summary: "On call: DB; nights\nweekends".into(),
            }],
        );
        assert!(ics.starts_with("BEGIN:VCALENDAR\r\nVERSION:2.0\r\n"));
        assert!(ics.ends_with("END:VEVENT\r\nEND:VCALENDAR\r\n"));
        assert!(ics.contains("X-WR-CALNAME:On call · Acme\\, Inc\r\n"));
        assert!(ics.contains(
            "DTSTAMP:20260926T100000Z\r\nDTSTART:20260928T060000Z\r\nDTEND:20261005T060000Z\r\n"
        ));
        assert!(ics.contains("SUMMARY:On call: DB\\; nights\\nweekends\r\nTRANSP:TRANSPARENT\r\n"));
    }

    #[test]
    fn long_lines_fold_without_splitting_a_character() {
        let mut out = String::new();
        push_line(&mut out, &format!("SUMMARY:{}", "ї".repeat(60)));
        let lines: Vec<&str> = out.trim_end_matches("\r\n").split("\r\n").collect();
        assert!(lines.len() > 1);
        assert!(lines.iter().all(|l| l.len() <= LINE_OCTETS));
        assert!(lines[1..].iter().all(|l| l.starts_with(' ')));
        let unfolded: String = out.replace("\r\n ", "");
        assert_eq!(unfolded, format!("SUMMARY:{}\r\n", "ї".repeat(60)));
    }

    #[test]
    fn control_characters_are_dropped() {
        assert_eq!(text("a\r\tb\u{7}c\\"), "abc\\\\");
    }
}
