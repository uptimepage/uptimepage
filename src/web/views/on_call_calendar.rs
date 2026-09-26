//! The schedule calendar on the on-call edit page: a month of days in the
//! schedule's timezone, each naming who is on call and from what time, with
//! overrides marked, and the overrides still to come listed for removal.
//!
//! Month navigation swaps the partial; adding or removing an override runs
//! against the JSON API and reloads it. Each day carries its own window as
//! instants, so the browser never does timezone math.

use askama::Template;
use askama_web::WebTemplate;
use axum::extract::{Path, Query, State};
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, Datelike, Days, Months, NaiveDate, NaiveTime, Utc};
use chrono_tz::Tz;
use serde::Deserialize;
use uuid::Uuid;

use crate::app::AppState;
use crate::domain::{
    FIRST_YEAR, LAST_YEAR, OnCallScheduleDetail, OnCallShift, local_to_utc, on_call_shifts,
};
use crate::error::{AppError, codes};
use crate::request::CurrentOrg;
use crate::templates::filters;
use crate::web::error::WebResult;
use crate::web::views::on_call::{Roster, org_members};
use crate::web::views::resolve_org;

/// Shifts listed in one day before the rest fold into a count.
const LINES_PER_DAY: usize = 4;

pub struct CalendarModel {
    pub schedule_id: String,
    /// The zone the days are cut in, as resolved rather than as stored.
    pub zone: &'static str,
    /// `2026-09`: the month shown, and the one a reload asks for.
    pub month: String,
    pub title: String,
    /// `None` at the edge of the months [`parse_month`] accepts.
    pub prev: Option<String>,
    pub next: Option<String>,
    /// Whole weeks, Monday first, covering the month.
    pub days: Vec<DayCell>,
    /// Overrides not yet over, soonest first.
    pub overrides: Vec<OverrideRow>,
}

pub struct DayCell {
    pub day: u32,
    /// `Sep 27`, naming the day in the override picker.
    pub label: String,
    pub in_month: bool,
    pub today: bool,
    /// Over already, so no override can start on it.
    pub past: bool,
    /// Local midnight to the next, as instants.
    pub starts_at: DateTime<Utc>,
    pub ends_at: DateTime<Utc>,
    pub lines: Vec<ShiftLine>,
    /// Shifts in the day beyond `lines`.
    pub more: usize,
}

/// One shift as a day shows it.
pub struct ShiftLine {
    /// Local time the shift starts, when that falls inside the day.
    pub from: Option<String>,
    /// Short names, empty when no one is on call.
    pub who: String,
    /// Full emails, each marked when no page can reach its owner.
    pub title: String,
    pub overridden: bool,
    /// Someone on it has no channel that can deliver a page.
    pub unreachable: bool,
}

pub struct OverrideRow {
    pub id: String,
    pub email: String,
    /// The window in the schedule's timezone.
    pub span: String,
}

#[derive(Template, WebTemplate)]
#[template(path = "settings/_on_call_calendar.html")]
pub struct CalendarPartial {
    pub cal: CalendarModel,
}

#[derive(Deserialize)]
pub struct CalendarQuery {
    #[serde(default)]
    month: Option<String>,
}

pub async fn partial(
    State(state): State<AppState>,
    org: Result<CurrentOrg, AppError>,
    Path(id): Path<Uuid>,
    Query(q): Query<CalendarQuery>,
) -> WebResult<Response> {
    let org = match resolve_org(org, &format!("/settings/on-call/{id}/edit")) {
        Ok(o) => o,
        Err(resp) => return Ok(*resp),
    };
    let month = q.month.as_deref().and_then(parse_month);
    let now = Utc::now();
    let (detail, members) = tokio::try_join!(
        async {
            Ok(state
                .on_call_store
                .get_from(org, id, overrides_from(month, now))
                .await?)
        },
        org_members(&state, org),
    )?;
    let detail = detail.ok_or_else(|| {
        AppError::not_found(codes::ON_CALL_SCHEDULE_NOT_FOUND, "schedule not found")
    })?;
    Ok(CalendarPartial {
        cal: calendar(&detail, &Roster::new(&members), month, now),
    }
    .into_response())
}

/// How far back [`calendar`] can need overrides for `month` (today's when
/// `None`): the grid's first day, a day early to absorb any zone's offset,
/// with today read a day early for the same reason.
pub fn overrides_from(month: Option<NaiveDate>, now: DateTime<Utc>) -> DateTime<Utc> {
    let (start, _) = grid(month.unwrap_or_else(|| (now - Days::new(1)).date_naive()));
    (start - Days::new(1))
        .and_time(NaiveTime::MIN)
        .and_utc()
        .min(now)
}

/// The first and last day of the whole weeks, Monday first, around the month
/// holding `day`.
fn grid(day: NaiveDate) -> (NaiveDate, NaiveDate) {
    let first = day.with_day(1).unwrap_or(day);
    let last = first + Months::new(1) - Days::new(1);
    (
        first - Days::new(u64::from(first.weekday().num_days_from_monday())),
        last + Days::new(u64::from(6 - last.weekday().num_days_from_monday())),
    )
}

/// `2026-09` as the first of that month. The last year's December grid would
/// run past [`LAST_YEAR`], so months stop a year short.
fn parse_month(raw: &str) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(&format!("{raw}-01"), "%Y-%m-%d")
        .ok()
        .filter(|d| (FIRST_YEAR..LAST_YEAR).contains(&d.year()))
}

/// Where `date` begins in `tz`: local midnight, or the first instant after
/// it when a DST change skips midnight.
fn day_start(tz: Tz, date: NaiveDate) -> DateTime<Utc> {
    local_to_utc(tz, date.and_time(NaiveTime::MIN))
}

/// The neighbouring month as a query value, when there is one to go to.
fn month_param(month: Option<NaiveDate>) -> Option<String> {
    month
        .filter(|m| (FIRST_YEAR..LAST_YEAR).contains(&m.year()))
        .map(|m| m.format("%Y-%m").to_string())
}

/// The month holding `month` (today's month in the schedule's zone when
/// `None`), resolved as of `now`.
pub fn calendar(
    detail: &OnCallScheduleDetail,
    roster: &Roster,
    month: Option<NaiveDate>,
    now: DateTime<Utc>,
) -> CalendarModel {
    let tz = detail.schedule.tz();
    let today = now.with_timezone(&tz).date_naive();
    let first = month.unwrap_or(today).with_day(1).unwrap_or(today);
    let (grid_start, grid_end) = grid(first);
    let dates: Vec<NaiveDate> = grid_start
        .iter_days()
        .take_while(|d| *d <= grid_end)
        .collect();
    let bounds: Vec<DateTime<Utc>> = dates
        .iter()
        .copied()
        .chain(std::iter::once(grid_end + Days::new(1)))
        .map(|d| day_start(tz, d))
        .collect();
    let shifts: Vec<OnCallShift> = on_call_shifts(
        &detail.schedule,
        &detail.layers,
        &detail.overrides,
        bounds[0],
        bounds[dates.len()],
    )
    .collect();
    let days = dates
        .iter()
        .zip(bounds.windows(2))
        .map(|(date, w)| {
            let (starts_at, ends_at) = (w[0], w[1]);
            let first_touching = shifts.partition_point(|s| s.ends_at <= starts_at);
            let mut touching = shifts[first_touching..]
                .iter()
                .take_while(|s| s.starts_at < ends_at);
            let lines = touching
                .by_ref()
                .take(LINES_PER_DAY)
                .map(|s| line(s, starts_at, tz, roster))
                .collect();
            DayCell {
                day: date.day(),
                label: date.format("%b %-d").to_string(),
                in_month: date.month() == first.month(),
                today: *date == today,
                past: ends_at <= now,
                starts_at,
                ends_at,
                lines,
                more: touching.count(),
            }
        })
        .collect();
    let mut upcoming: Vec<_> = detail
        .overrides
        .iter()
        .filter(|o| o.ends_at > now)
        .collect();
    upcoming.sort_by_key(|o| o.starts_at);
    CalendarModel {
        schedule_id: detail.schedule.id.to_string(),
        zone: tz.name(),
        month: first.format("%Y-%m").to_string(),
        title: first.format("%B %Y").to_string(),
        prev: month_param(first.checked_sub_months(Months::new(1))),
        next: month_param(first.checked_add_months(Months::new(1))),
        days,
        overrides: upcoming
            .into_iter()
            .map(|o| OverrideRow {
                id: o.id.to_string(),
                email: roster.email(&o.user_id).to_owned(),
                span: span(o.starts_at, o.ends_at, tz, today.year()),
            })
            .collect(),
    }
}

/// An override's window as local dates: whole days as the inclusive range
/// the picker offered, anything else with its times. A year other than this
/// one is written out.
fn span(starts: DateTime<Utc>, ends: DateTime<Utc>, tz: Tz, this_year: i32) -> String {
    let (s, e) = (starts.with_timezone(&tz), ends.with_timezone(&tz));
    let whole = |at: DateTime<Utc>, local: NaiveDate| at == day_start(tz, local);
    let date = |d: NaiveDate| {
        let fmt = if d.year() == this_year {
            "%b %-d"
        } else {
            "%b %-d, %Y"
        };
        d.format(fmt).to_string()
    };
    let last = e.date_naive() - Days::new(1);
    match (whole(starts, s.date_naive()), whole(ends, e.date_naive())) {
        (true, true) if last == s.date_naive() => date(s.date_naive()),
        (true, true) => format!("{}–{}", date(s.date_naive()), date(last)),
        // A day picked whole ends at its 24:00, not the next day's 00:00.
        (_, true) => format!(
            "{} {} → {} 24:00",
            date(s.date_naive()),
            s.format("%H:%M"),
            date(last)
        ),
        _ => format!(
            "{} {} → {} {}",
            date(s.date_naive()),
            s.format("%H:%M"),
            date(e.date_naive()),
            e.format("%H:%M")
        ),
    }
}

fn line(shift: &OnCallShift, day_start: DateTime<Utc>, tz: Tz, roster: &Roster) -> ShiftLine {
    let who: Vec<&str> = shift.user_ids.iter().map(|u| roster.short(u)).collect();
    let title: Vec<String> = shift
        .user_ids
        .iter()
        .map(|u| match roster.email(u) {
            email if roster.reachable(u) => email.to_owned(),
            email => format!("{email} (no paging channels)"),
        })
        .collect();
    ShiftLine {
        from: (shift.starts_at > day_start).then(|| {
            shift
                .starts_at
                .with_timezone(&tz)
                .format("%H:%M")
                .to_string()
        }),
        who: who.join(", "),
        title: title.join(", "),
        overridden: shift.overridden,
        unreachable: shift.user_ids.iter().any(|u| !roster.reachable(u)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{
        OnCallLayer, OnCallOverride, OnCallParticipant, OnCallSchedule, RotationType, UserId,
    };
    use crate::web::views::on_call::MemberChoice;

    fn t(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    fn uid(n: u128) -> UserId {
        UserId(Uuid::from_u128(n))
    }

    fn members() -> Vec<MemberChoice> {
        vec![
            MemberChoice {
                id: uid(1),
                email: "olena@example.com".into(),
                reachable: true,
            },
            MemberChoice {
                id: uid(2),
                email: "taras@example.com".into(),
                reachable: false,
            },
            MemberChoice {
                id: uid(3),
                email: "iryna@example.com".into(),
                reachable: true,
            },
        ]
    }

    fn detail(tz: &str, overrides: Vec<OnCallOverride>) -> OnCallScheduleDetail {
        OnCallScheduleDetail {
            schedule: OnCallSchedule {
                id: Uuid::nil(),
                name: "Primary".into(),
                timezone: tz.into(),
                created_at: t("2026-01-01T00:00:00Z"),
                updated_at: t("2026-01-01T00:00:00Z"),
            },
            layers: vec![OnCallLayer {
                id: Uuid::now_v7(),
                name: None,
                rotation_type: RotationType::Weekly,
                rotation_length_secs: 604_800,
                handoff_at: t("2026-09-07T06:00:00Z"), // Monday 09:00 in Kyiv
                layer_order: 0,
                created_at: t("2026-01-01T00:00:00Z"),
                participants: vec![
                    OnCallParticipant {
                        id: Uuid::now_v7(),
                        user_id: uid(1),
                        position: 0,
                    },
                    OnCallParticipant {
                        id: Uuid::now_v7(),
                        user_id: uid(2),
                        position: 1,
                    },
                ],
            }],
            overrides,
        }
    }

    fn cover(user: UserId, from: &str, to: &str) -> OnCallOverride {
        OnCallOverride {
            id: Uuid::now_v7(),
            user_id: user,
            starts_at: t(from),
            ends_at: t(to),
            created_by: None,
            created_at: t("2026-09-01T00:00:00Z"),
        }
    }

    fn day<'a>(cal: &'a CalendarModel, label: &str) -> &'a DayCell {
        cal.days.iter().find(|d| d.label == label).unwrap()
    }

    fn september(overrides: Vec<OnCallOverride>) -> CalendarModel {
        let members = members();
        calendar(
            &detail("Europe/Kyiv", overrides),
            &Roster::new(&members),
            parse_month("2026-09"),
            t("2026-09-26T10:00:00Z"),
        )
    }

    #[test]
    fn the_grid_runs_whole_weeks_from_monday() {
        let cal = september(vec![]);
        assert_eq!(cal.days.len(), 35);
        assert_eq!(cal.days[0].label, "Aug 31");
        assert!(!cal.days[0].in_month);
        assert_eq!(cal.days[34].label, "Oct 4");
        assert_eq!(cal.prev.as_deref(), Some("2026-08"));
        assert_eq!(cal.next.as_deref(), Some("2026-10"));
        assert_eq!(cal.title, "September 2026");
        assert_eq!(cal.zone, "Europe/Kyiv");
        assert!(day(&cal, "Sep 26").today);
        assert!(!day(&cal, "Sep 26").past, "today is still open");
        assert!(day(&cal, "Sep 25").past);
        assert!(!day(&cal, "Sep 27").past);
    }

    #[test]
    fn days_run_midnight_to_midnight_in_the_schedule_zone() {
        let cal = september(vec![]);
        let d = day(&cal, "Sep 14");
        assert_eq!(d.starts_at, t("2026-09-13T21:00:00Z"));
        assert_eq!(d.ends_at, t("2026-09-14T21:00:00Z"));
    }

    #[test]
    fn a_handoff_day_names_both_and_the_local_time() {
        let cal = september(vec![]);
        let d = day(&cal, "Sep 14");
        let lines: Vec<_> = d
            .lines
            .iter()
            .map(|l| (l.from.as_deref(), l.who.as_str()))
            .collect();
        assert_eq!(lines, vec![(None, "olena"), (Some("09:00"), "taras")]);
        assert!(!d.lines[0].unreachable);
        assert!(d.lines[1].unreachable);
        assert_eq!(d.lines[0].title, "olena@example.com");
        let quiet = day(&cal, "Sep 16");
        assert_eq!(quiet.lines.len(), 1);
        assert_eq!(quiet.lines[0].who, "taras");
    }

    #[test]
    fn overrides_are_marked_and_the_ones_to_come_listed() {
        let cal = september(vec![
            cover(uid(3), "2026-09-27T21:00:00Z", "2026-09-29T21:00:00Z"),
            cover(uid(3), "2026-09-01T21:00:00Z", "2026-09-02T21:00:00Z"),
        ]);
        let d = day(&cal, "Sep 29");
        assert_eq!(d.lines.len(), 1);
        assert!(d.lines[0].overridden);
        assert_eq!(d.lines[0].who, "iryna");
        assert!(day(&cal, "Sep 2").lines[0].overridden);
        assert_eq!(cal.overrides.len(), 1, "a past override is not listed");
        assert_eq!(cal.overrides[0].span, "Sep 28–Sep 29");
    }

    #[test]
    fn a_crowded_day_folds_into_a_count() {
        let overrides = (0..6)
            .map(|h| {
                cover(
                    uid(3),
                    &format!("2026-09-16T{:02}:00:00Z", h * 2),
                    &format!("2026-09-16T{:02}:00:00Z", h * 2 + 1),
                )
            })
            .collect();
        let cal = september(overrides);
        let d = day(&cal, "Sep 16");
        assert_eq!(d.lines.len(), LINES_PER_DAY);
        assert_eq!(d.more, 13 - LINES_PER_DAY);
    }

    #[test]
    fn spans_read_as_whole_days_or_with_times_and_name_another_year() {
        let kyiv = chrono_tz::Europe::Kyiv;
        let one_day = span(
            t("2026-09-27T21:00:00Z"),
            t("2026-09-28T21:00:00Z"),
            kyiv,
            2026,
        );
        assert_eq!(one_day, "Sep 28");
        let over_new_year = span(
            t("2026-12-29T22:00:00Z"),
            t("2027-01-02T22:00:00Z"),
            kyiv,
            2026,
        );
        assert_eq!(over_new_year, "Dec 30–Jan 2, 2027");
        let timed = span(
            t("2026-09-28T06:00:00Z"),
            t("2026-09-28T15:00:00Z"),
            kyiv,
            2026,
        );
        assert_eq!(timed, "Sep 28 09:00 → Sep 28 18:00");
        let begun_today = span(
            t("2026-09-26T11:03:00Z"),
            t("2026-09-27T21:00:00Z"),
            kyiv,
            2026,
        );
        assert_eq!(begun_today, "Sep 26 14:03 → Sep 27 24:00");
    }

    #[test]
    fn a_day_whose_midnight_is_skipped_still_reads_as_a_whole_day() {
        let santiago = chrono_tz::America::Santiago;
        let sep = |d| NaiveDate::from_ymd_opt(2026, 9, d).unwrap();
        // Clocks jump from 00:00 to 01:00 on Sep 6.
        assert_eq!(day_start(santiago, sep(6)), t("2026-09-06T04:00:00Z"));
        assert_eq!(
            span(
                day_start(santiago, sep(6)),
                day_start(santiago, sep(8)),
                santiago,
                2026
            ),
            "Sep 6–Sep 7"
        );
    }

    #[test]
    fn a_line_of_several_marks_only_the_one_no_page_reaches() {
        let members = members();
        let roster = Roster::new(&members);
        let shift = OnCallShift {
            starts_at: t("2026-09-16T00:00:00Z"),
            ends_at: t("2026-09-16T06:00:00Z"),
            user_ids: vec![uid(1), uid(2)],
            overridden: true,
        };
        let l = line(
            &shift,
            t("2026-09-15T21:00:00Z"),
            chrono_tz::Europe::Kyiv,
            &roster,
        );
        assert_eq!(l.who, "olena, taras");
        assert_eq!(
            l.title,
            "olena@example.com, taras@example.com (no paging channels)"
        );
        assert_eq!(l.from.as_deref(), Some("03:00"));
    }

    #[test]
    fn navigation_stops_at_the_months_it_accepts() {
        let members = members();
        let cal = calendar(
            &detail("UTC", vec![]),
            &Roster::new(&members),
            parse_month("1970-01"),
            t("2026-09-26T10:00:00Z"),
        );
        assert_eq!(cal.prev, None);
        assert_eq!(cal.next.as_deref(), Some("1970-02"));
        let last = calendar(
            &detail("UTC", vec![]),
            &Roster::new(&members),
            parse_month("9998-12"),
            t("2026-09-26T10:00:00Z"),
        );
        assert_eq!(last.next, None);
        assert_eq!(last.days.last().unwrap().ends_at.year(), 9999);
    }

    #[test]
    fn an_unknown_zone_is_labelled_as_the_utc_it_falls_back_to() {
        let members = members();
        let cal = calendar(
            &detail("Mars/Phobos", vec![]),
            &Roster::new(&members),
            None,
            t("2026-09-26T10:00:00Z"),
        );
        assert_eq!(cal.zone, "UTC");
    }

    #[test]
    fn overrides_load_from_before_any_grid_day_in_any_zone() {
        let now = t("2026-10-01T01:00:00Z");
        // Still September in New York, so its grid starts Aug 31.
        assert_eq!(overrides_from(None, now), t("2026-08-30T00:00:00Z"));
        assert_eq!(
            overrides_from(parse_month("2026-12"), now),
            t("2026-10-01T01:00:00Z"),
            "the overrides still to come are listed whatever month is shown"
        );
        assert_eq!(
            overrides_from(parse_month("2026-03"), now),
            t("2026-02-22T00:00:00Z")
        );
    }

    #[test]
    fn a_bad_month_falls_back_to_today() {
        assert!(parse_month("2026-13").is_none());
        assert!(parse_month("x").is_none());
        assert!(parse_month("99999-01").is_none());
        assert!(parse_month("9999-12").is_none());
        assert_eq!(parse_month("2026-02"), NaiveDate::from_ymd_opt(2026, 2, 1));
    }

    #[test]
    fn partial_renders_days_and_the_override_list() {
        let html = CalendarPartial {
            cal: september(vec![cover(
                uid(3),
                "2026-09-27T21:00:00Z",
                "2026-09-29T21:00:00Z",
            )]),
        }
        .render()
        .unwrap();
        assert!(html.contains(r#"data-month="2026-09""#));
        assert!(
            html.contains(r#"data-start="2026-09-13T21:00:00Z" data-end="2026-09-14T21:00:00Z""#)
        );
        assert!(html.contains("?month=2026-10"));
        assert!(html.contains(r#"data-override-remove="#));
        assert!(html.contains("Sep 28–Sep 29"));
        assert!(html.contains(r#"title="taras@example.com (no paging channels)""#));
    }
}
