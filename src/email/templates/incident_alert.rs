//! Incident alert mail. Takes the incident's fields, not a rendered line, so the
//! card can lay out state, timing and regions the way chat transports cannot.

use chrono::{DateTime, Utc};

use crate::domain::{IncidentOrigin, IncidentSeverity, IncidentUrgency, NotificationReason};
use crate::email::templates::layout::{self, ButtonStyle, Page, Tone};
use crate::email::templates::{html_escape, single_line, utc_stamp};
use crate::email::trait_def::RenderedEmail;

/// Longer than one line of machine output belongs in a block of its own, where
/// wrapping is expected and a stack trace stays readable.
const INLINE_REASON_CHARS: usize = 120;

/// Everything the alert card renders. `summary` is the transport-shared one-line
/// wording, kept as the subject so an inbox and a chat channel agree.
#[derive(Debug, Clone)]
pub struct IncidentAlert {
    pub summary: String,
    pub label: String,
    pub reason: NotificationReason,
    pub severity: IncidentSeverity,
    pub urgency: IncidentUrgency,
    pub origin: IncidentOrigin,
    pub started_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
    pub error_sample: Option<String>,
    pub regions_down: Vec<String>,
    pub regions_up: Vec<String>,
    pub url: Option<String>,
    pub note: Option<String>,
    pub org_name: Option<String>,
    pub stop_url: Option<String>,
}

impl IncidentAlert {
    fn tone(&self) -> Tone {
        match self.reason {
            NotificationReason::Resolved | NotificationReason::DataResumed => Tone::Good,
            NotificationReason::NoData => Tone::Info,
            _ => match self.severity {
                IncidentSeverity::Minor => Tone::Warn,
                _ => Tone::Bad,
            },
        }
    }

    /// Upper-cased here, not by CSS: Outlook's Word engine ignores
    /// `text-transform`, and this line is the one that must read as a state.
    fn kicker(&self) -> String {
        let sev = self.severity.as_db_str().to_uppercase();
        match self.reason {
            NotificationReason::Opened => format!("{sev} INCIDENT OPEN"),
            NotificationReason::Escalated => format!("{sev} INCIDENT ESCALATED"),
            NotificationReason::Reopened => format!("{sev} INCIDENT REOPENED"),
            NotificationReason::Resolved => "INCIDENT RESOLVED".into(),
            NotificationReason::NoData => "MONITORING INTERRUPTED".into(),
            NotificationReason::DataResumed => "MONITORING RESUMED".into(),
            NotificationReason::Reminder => format!("{sev} INCIDENT STILL OPEN"),
        }
    }

    /// The state in one sentence, under the monitor name.
    fn strapline(&self) -> String {
        match self.reason {
            NotificationReason::Resolved => match self.duration() {
                Some(d) => format!("Recovered after {d}"),
                None => "Recovered".into(),
            },
            NotificationReason::NoData => "No check results are arriving".into(),
            NotificationReason::DataResumed => "Check results are arriving again".into(),
            // No failing check behind it, so the detection wording would be a
            // claim the product cannot support.
            _ if self.origin == IncidentOrigin::Manual => {
                format!("Declared by hand at {}", utc_stamp(self.started_at))
            }
            _ => match self.region_counts() {
                Some((down, total)) => format!("Failing in {down} of {total} regions"),
                None => format!("Failing since {}", utc_stamp(self.started_at)),
            },
        }
    }

    /// An incident is in play right now, so its severity, urgency, failure text
    /// and region breakdown still describe the present. Deliberately excludes
    /// `NoData` and `DataResumed`: monitoring stopping or restarting is not an
    /// incident, carries no severity of its own, and would otherwise show the
    /// last incident's numbers as if they were current.
    fn is_open(&self) -> bool {
        matches!(
            self.reason,
            NotificationReason::Opened
                | NotificationReason::Escalated
                | NotificationReason::Reopened
                | NotificationReason::Reminder
        )
    }

    /// Whether `started_at` is a moment something began. For `NoData` and
    /// `DataResumed` the notifier stamps it at send time, so printing it under
    /// "Started" reads as when the gap opened rather than when the mail went
    /// out. Resolved is excluded from `is_open` but does have a real start, so
    /// this cannot borrow that predicate.
    fn has_incident_window(&self) -> bool {
        !matches!(
            self.reason,
            NotificationReason::NoData | NotificationReason::DataResumed
        )
    }

    /// `(down, total)`, only for a monitor watched from more than one region.
    /// The breakdown is what the incident held when this mail was built, so it
    /// describes the present only while the incident is open.
    fn region_counts(&self) -> Option<(usize, usize)> {
        if !self.is_open() {
            return None;
        }
        let total = self.regions_down.len() + self.regions_up.len();
        (total > 1).then_some((self.regions_down.len(), total))
    }

    fn duration(&self) -> Option<String> {
        let end = self.ended_at?;
        Some(crate::email::templates::duration_words(
            (end - self.started_at).num_seconds(),
        ))
    }

    /// The failure as a single scannable line, when it is one. Longer or
    /// multi-line output goes to a block of its own instead.
    fn inline_reason(&self) -> Option<&str> {
        self.error_sample.as_deref().filter(|e| {
            self.is_open()
                && !e.contains('\n')
                && !e.contains('\r')
                && e.chars().count() <= INLINE_REASON_CHARS
        })
    }

    /// The failure text that did not fit on one line.
    fn block_reason(&self) -> Option<&str> {
        match self.inline_reason() {
            Some(_) => None,
            // A closed incident's error sample describes what already ended.
            None => self.error_sample.as_deref().filter(|_| self.is_open()),
        }
    }

    fn facts(&self) -> Vec<(&'static str, String)> {
        let mut rows: Vec<(&'static str, String)> = Vec::new();
        if let Some(reason) = self.inline_reason() {
            rows.push(("Reason", reason.to_string()));
        }
        if self.is_open() {
            rows.push(("Severity", self.severity.as_db_str().to_string()));
            rows.push(("Urgency", self.urgency.as_db_str().to_string()));
            if self.origin == IncidentOrigin::Manual {
                rows.push(("Origin", "declared by hand, no monitor detection".into()));
            }
        }
        if self.has_incident_window() {
            rows.push(("Started", utc_stamp(self.started_at)));
        }
        if let Some(end) = self.ended_at {
            rows.push(("Ended", utc_stamp(end)));
            if let Some(duration) = self.duration() {
                rows.push(("Lasted", duration));
            }
        }
        if self.region_counts().is_some() {
            rows.push(("Down in", join_regions(&self.regions_down)));
            if !self.regions_up.is_empty() {
                rows.push(("Not confirmed", join_regions(&self.regions_up)));
            }
        }
        rows
    }

    /// Inbox preview line. Carries what the subject had to leave out.
    fn preheader(&self) -> String {
        let mut parts = vec![self.strapline()];
        if self.region_counts().is_some() && !self.regions_down.is_empty() {
            parts.push(format!("down: {}", join_regions(&self.regions_down)));
        }
        if let Some(reason) = self.inline_reason() {
            parts.push(reason.to_string());
        }
        single_line(&parts.join(" · "))
    }
}

fn join_regions(regions: &[String]) -> String {
    if regions.is_empty() {
        return "none".into();
    }
    regions.join(", ")
}

pub fn render(site_name: &str, alert: &IncidentAlert) -> RenderedEmail {
    let subject = alert.summary.clone();
    let facts = alert.facts();

    let mut text = format!("{subject}\n");
    for (label, value) in &facts {
        text.push_str(&format!("{label}: {value}\n"));
    }
    if let Some(error) = alert.block_reason() {
        text.push_str(&format!("\n{error}\n"));
    }
    if let Some(url) = &alert.url {
        text.push_str(&format!("\n{url}\n"));
    }
    if let Some(note) = &alert.note {
        text.push_str(&format!("\n{note}\n"));
    }
    let mut footer_text = String::new();
    if let Some(org) = &alert.org_name {
        footer_text.push_str(&format!(
            "You're receiving this because {org} added this address as an alert \
             channel on {site_name}.\n"
        ));
    }
    if let Some(url) = &alert.stop_url {
        footer_text.push_str(&format!("Stop delivery to this address: {url}\n"));
    }
    // One blank line before whichever footer line comes first, so the footer
    // never runs into the last fact row.
    if !footer_text.is_empty() {
        text.push('\n');
        text.push_str(&footer_text);
    }
    text.push_str(&format!("\n— {site_name} alerts\n"));

    let mut body = layout::facts(&facts);
    if let Some(error) = alert.block_reason() {
        body.push_str(&layout::fine_print("What the check reported"));
        body.push_str(&layout::code_block(error));
    }
    if let Some(note) = &alert.note {
        body.push_str(&layout::callout(note));
    }
    if let Some(url) = &alert.url {
        body.push_str(&layout::button(url, "View incident", ButtonStyle::Outline));
    }

    let footnote = {
        let mut parts = String::new();
        if let Some(org) = &alert.org_name {
            parts.push_str(&format!(
                "You're receiving this because <strong>{org}</strong> added this address \
                 as an alert channel on {site}.",
                org = html_escape(org),
                site = html_escape(site_name),
            ));
        }
        if let Some(url) = &alert.stop_url {
            parts.push(' ');
            parts.push_str(&layout::quiet_link(url, "Stop delivery to this address"));
            parts.push('.');
        }
        (!parts.trim().is_empty()).then(|| layout::fine_print(parts.trim()))
    };

    let html_body = layout::render(Page {
        title: &subject,
        preheader: &alert.preheader(),
        signature: Some(site_name),
        header: layout::band(
            alert.tone(),
            &alert.kicker(),
            &alert.label,
            Some(&alert.strapline()),
        ),
        body,
        footnote,
    });

    RenderedEmail {
        subject,
        text_body: text,
        html_body,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn alert(reason: NotificationReason) -> IncidentAlert {
        IncidentAlert {
            summary: "api.acme.test — major incident OPEN: no response".into(),
            label: "api.acme.test".into(),
            reason,
            severity: IncidentSeverity::Major,
            urgency: IncidentUrgency::High,
            origin: IncidentOrigin::Monitor,
            started_at: Utc.with_ymd_and_hms(2026, 8, 17, 12, 41, 0).unwrap(),
            ended_at: None,
            error_sample: Some("no response".into()),
            regions_down: vec!["apac-sg".into(), "eu-helsinki".into()],
            regions_up: vec!["us-east".into()],
            url: Some("https://app.test/incidents/7".into()),
            note: None,
            org_name: Some("My status".into()),
            stop_url: Some("https://app.test/alert-channel/stop?c=1&t=2".into()),
        }
    }

    #[test]
    fn subject_is_the_transport_shared_summary() {
        let r = render("Uptimepage", &alert(NotificationReason::Opened));
        assert_eq!(
            r.subject,
            "api.acme.test — major incident OPEN: no response"
        );
        assert!(r.text_body.starts_with(&r.subject));
    }

    #[test]
    fn subject_never_spans_multiple_lines() {
        // Flattening lives in `EmailTemplate::render`, which every send goes
        // through, so that is the level the guarantee is asserted at.
        let mut a = alert(NotificationReason::Opened);
        a.summary = "api — down\r\nBcc: evil@example.test".into();
        let r = crate::email::EmailTemplate::IncidentAlert(a).render("Uptimepage");
        assert!(!r.subject.contains('\n') && !r.subject.contains('\r'));
        assert!(r.subject.contains("Bcc: evil@example.test"), "text kept");
    }

    #[test]
    fn state_timing_and_regions_reach_both_bodies() {
        let r = render("Uptimepage", &alert(NotificationReason::Opened));
        for body in [&r.text_body, &r.html_body] {
            assert!(body.contains("17 Aug 2026 12:41 UTC"), "start time");
            assert!(body.contains("apac-sg, eu-helsinki"), "regions down");
            assert!(body.contains("us-east"), "regions not confirmed");
            assert!(body.contains("high"), "urgency");
        }
        assert!(r.html_body.contains("MAJOR INCIDENT OPEN"));
        assert!(r.html_body.contains("Failing in 2 of 3 regions"));
    }

    /// Reading like a detection tells an operator the product fired on a
    /// healthy site.
    #[test]
    fn a_declared_incident_says_so_instead_of_claiming_a_detection() {
        let mut a = alert(NotificationReason::Opened);
        a.origin = IncidentOrigin::Manual;
        a.error_sample = None;
        a.regions_down = Vec::new();
        a.regions_up = Vec::new();
        let r = render("Uptimepage", &a);
        for body in [&r.text_body, &r.html_body] {
            assert!(body.contains("declared by hand"), "{body}");
            assert!(!body.contains("Failing"), "{body}");
        }
        assert!(
            r.html_body
                .contains("Declared by hand at 17 Aug 2026 12:41 UTC")
        );
    }

    #[test]
    fn a_monitor_detection_never_mentions_being_declared() {
        let r = render("Uptimepage", &alert(NotificationReason::Opened));
        assert!(!r.text_body.contains("declared by hand"), "{}", r.text_body);
        assert!(r.html_body.contains("Failing in 2 of 3 regions"));
    }

    #[test]
    fn resolved_alert_reports_how_long_it_lasted() {
        let mut a = alert(NotificationReason::Resolved);
        a.summary = "api.acme.test — incident RESOLVED after 82m".into();
        a.ended_at = Some(a.started_at + chrono::Duration::minutes(82));
        let r = render("Uptimepage", &a);
        for body in [&r.text_body, &r.html_body] {
            assert!(body.contains("1h 22m"), "duration: {body}");
        }
        assert!(r.html_body.contains("INCIDENT RESOLVED"));
        assert!(
            !r.html_body.to_lowercase().contains("urgency"),
            "a closed incident has nothing left to page about"
        );
    }

    #[test]
    fn a_resolved_alert_never_reports_regions_as_still_down() {
        // The notice carries the incident's open-time region snapshot for every
        // reason, resolves included, so a closed alert must not read it as now.
        let mut a = alert(NotificationReason::Resolved);
        a.summary = "api.acme.test — incident RESOLVED after 82m".into();
        a.ended_at = Some(a.started_at + chrono::Duration::minutes(82));
        let r = render("Uptimepage", &a);
        for body in [&r.text_body, &r.html_body] {
            // Case-insensitive: the HTML labels ship upper-cased, the text ones do not.
            let body = body.to_lowercase();
            assert!(!body.contains("down in"), "stale breakdown: {body}");
            assert!(!body.contains("not confirmed"), "stale breakdown: {body}");
            assert!(!body.contains("no response"), "stale failure: {body}");
        }
    }

    #[test]
    fn a_single_region_monitor_skips_the_region_breakdown() {
        let mut a = alert(NotificationReason::Opened);
        a.regions_down = vec!["eu-helsinki".into()];
        a.regions_up = Vec::new();
        let r = render("Uptimepage", &a);
        assert!(!r.html_body.to_lowercase().contains("not confirmed"));
        assert!(r.html_body.contains("Failing since 17 Aug 2026 12:41 UTC"));
    }

    #[test]
    fn a_long_error_moves_out_of_the_row_into_a_block() {
        let mut a = alert(NotificationReason::Opened);
        a.error_sample = Some("x".repeat(INLINE_REASON_CHARS + 1));
        let r = render("Uptimepage", &a);
        assert!(r.html_body.contains("What the check reported"));
        assert!(r.html_body.contains(&"x".repeat(INLINE_REASON_CHARS + 1)));
        assert!(
            !r.html_body.to_lowercase().contains(">reason<"),
            "not also a fact row"
        );
    }

    #[test]
    fn a_multiline_error_keeps_its_line_breaks_out_of_the_table() {
        let mut a = alert(NotificationReason::Opened);
        a.error_sample = Some("line one\nline two".into());
        let r = render("Uptimepage", &a);
        assert!(r.html_body.contains("line one\nline two"));
        assert!(r.text_body.contains("line one\nline two"));
    }

    #[test]
    fn a_held_alert_note_is_carried_as_an_aside() {
        let mut a = alert(NotificationReason::Opened);
        a.note = Some("Further alerts are held while this monitor flaps.".into());
        let r = render("Uptimepage", &a);
        assert!(r.text_body.contains("Further alerts are held"));
        assert!(r.html_body.contains("Further alerts are held"));
    }

    #[test]
    fn attribution_and_stop_link_appear_when_present() {
        let r = render("Uptimepage", &alert(NotificationReason::Opened));
        assert!(r.text_body.contains("My status added this address"));
        assert!(r.text_body.contains("/alert-channel/stop?c=1&t=2"));
        assert!(r.html_body.contains("<strong>My status</strong>"));
        assert!(r.html_body.contains("Stop delivery to this address"));
    }

    #[test]
    fn no_footer_lines_without_context() {
        let mut a = alert(NotificationReason::Opened);
        a.org_name = None;
        a.stop_url = None;
        let r = render("Uptimepage", &a);
        assert!(!r.text_body.contains("You're receiving"));
        assert!(!r.text_body.contains("Stop delivery"));
        assert!(!r.html_body.contains("Stop delivery"));
    }

    #[test]
    fn a_monitor_name_cannot_inject_markup() {
        let mut a = alert(NotificationReason::Opened);
        a.label = "<img src=x onerror=alert(1)>".into();
        let r = render("Uptimepage", &a);
        assert!(!r.html_body.contains("<img src=x"));
        assert!(r.html_body.contains("&lt;img src=x"));
    }

    /// The notifier stamps `started_at` at send time for these two, so a
    /// "Started" row would date the interruption to the moment the mail went
    /// out. The plain-text body it replaced carried no timestamp at all, so
    /// this would be new misinformation rather than an inherited one.
    #[test]
    fn monitoring_gaps_do_not_claim_a_start_time() {
        for reason in [NotificationReason::NoData, NotificationReason::DataResumed] {
            let r = render("Uptimepage", &alert(reason));
            for body in [&r.text_body, &r.html_body] {
                // The card uppercases its labels, so a case-sensitive check
                // would pass against the HTML without reading it.
                assert!(
                    !body.to_lowercase().contains("started"),
                    "{reason:?} dates the gap to send time"
                );
                assert!(
                    !body.contains("17 Aug 2026 12:41 UTC"),
                    "{reason:?} prints the send stamp as a fact"
                );
            }
        }
    }

    /// A resolved incident is not open, but it did start, so suppressing the
    /// row cannot key on `is_open`.
    #[test]
    fn a_closed_incident_still_reports_when_it_began() {
        let mut a = alert(NotificationReason::Resolved);
        a.ended_at = Some(Utc.with_ymd_and_hms(2026, 8, 17, 13, 5, 0).unwrap());
        let r = render("Uptimepage", &a);
        for body in [&r.text_body, &r.html_body] {
            assert!(
                body.to_lowercase().contains("started"),
                "a resolved incident lost its start"
            );
            assert!(body.contains("17 Aug 2026 12:41 UTC"));
        }
    }
}
