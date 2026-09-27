//! A member's own on-call shifts: the next few on the on-call page, and the
//! ones around now as a calendar feed their calendar app subscribes to.
//!
//! The feed is public: its link carries a secret token, as a share link does,
//! so an app can fetch it without signing in. A new link, or leaving the org,
//! ends the old one.

use askama::Template;
use askama_web::WebTemplate;
use axum::extract::{Path, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, NaiveTime, TimeDelta, Utc};

use crate::app::AppState;
use crate::domain::{OnCallScheduleDetail, UserId, shifts_held_by};
use crate::error::AppError;
use crate::request::{AuthedBrowser, CurrentOrg, CurrentUser};
use crate::storage::on_call_feeds::{self, FeedOwner, feed_url};
use crate::templates::filters;
use crate::web::error::WebResult;
use crate::web::ical::{Event, calendar};
use crate::web::views::resolve_org;

/// How far back the feed reaches, so a calendar keeps the recent shifts.
const FEED_PAST_DAYS: i64 = 30;
/// How far ahead the page and the feed look.
const AHEAD_DAYS: i64 = 90;
/// Shifts the page lists; the feed has the rest.
const SHIFTS_SHOWN: usize = 5;

pub struct MyShift {
    pub schedule: String,
    /// Already on when the page was drawn.
    pub now: bool,
    pub starts_at: DateTime<Utc>,
    /// `None` when no one takes over within [`AHEAD_DAYS`].
    pub ends_at: Option<DateTime<Utc>>,
}

#[derive(Template, WebTemplate)]
#[template(path = "settings/_on_call_shifts.html")]
pub struct ShiftsPartial {
    pub shifts: Vec<MyShift>,
    /// The feed link, once the member has made one.
    pub feed_url: Option<String>,
}

pub async fn partial(
    _auth: AuthedBrowser,
    CurrentUser(user): CurrentUser,
    State(state): State<AppState>,
    org: Result<CurrentOrg, AppError>,
) -> WebResult<Response> {
    let org = match resolve_org(org, "/settings/on-call") {
        Ok(o) => o,
        Err(resp) => return Ok(*resp),
    };
    let now = Utc::now();
    let pool = state.require_db()?;
    let (schedules, token) = tokio::try_join!(
        state.on_call_store.current(org, now),
        on_call_feeds::token(pool, state.cipher.as_deref(), org, user),
    )?;
    Ok(ShiftsPartial {
        shifts: upcoming(&schedules, user, now),
        feed_url: token.map(|t| feed_url(&state.cfg.auth.public_base_url, &t)),
    }
    .into_response())
}

/// The member's next [`SHIFTS_SHOWN`] shifts across every schedule, soonest
/// first.
fn upcoming(schedules: &[OnCallScheduleDetail], user: UserId, now: DateTime<Utc>) -> Vec<MyShift> {
    let horizon = now + TimeDelta::days(AHEAD_DAYS);
    let mut out: Vec<MyShift> = schedules
        .iter()
        .flat_map(|d| {
            shifts_held_by(&d.schedule, &d.layers, &d.overrides, user, now, horizon)
                .take(SHIFTS_SHOWN)
                .map(|r| MyShift {
                    schedule: d.schedule.name.clone(),
                    now: r.start == now,
                    starts_at: r.start,
                    ends_at: (r.end < horizon).then_some(r.end),
                })
        })
        .collect();
    out.sort_by_key(|s| s.starts_at);
    out.truncate(SHIFTS_SHOWN);
    out
}

/// `/ical/{token}.ics`. Unknown tokens and deleted orgs 404 alike.
pub async fn feed(State(state): State<AppState>, Path(file): Path<String>) -> Response {
    let Some(token) = file.strip_suffix(".ics") else {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    };
    match feed_body(&state, token, Utc::now()).await {
        Ok(Some(body)) => (
            [
                (header::CONTENT_TYPE, "text/calendar; charset=utf-8"),
                (header::CACHE_CONTROL, "no-store"),
            ],
            body,
        )
            .into_response(),
        Ok(None) => (StatusCode::NOT_FOUND, "not found").into_response(),
        Err(err) => {
            tracing::error!(error = %err, "on-call feed failed");
            (StatusCode::SERVICE_UNAVAILABLE, "try again").into_response()
        }
    }
}

async fn feed_body(
    state: &AppState,
    token: &str,
    now: DateTime<Utc>,
) -> crate::error::Result<Option<String>> {
    let Some(owner) = on_call_feeds::resolve(state.require_db()?, token).await? else {
        return Ok(None);
    };
    let (from, to) = feed_window(now);
    let schedules = state.on_call_store.current(owner.org, from).await?;
    Ok(Some(ical(&schedules, &owner, from, to, now)))
}

/// The feed's reach around `now`, cut at UTC midnights so an event clipped at
/// either edge keeps its times, and so its UID, through a day of fetches.
fn feed_window(now: DateTime<Utc>) -> (DateTime<Utc>, DateTime<Utc>) {
    let midnight = |at: DateTime<Utc>| at.date_naive().and_time(NaiveTime::MIN).and_utc();
    (
        midnight(now - TimeDelta::days(FEED_PAST_DAYS)),
        midnight(now + TimeDelta::days(AHEAD_DAYS)),
    )
}

/// The owner's shifts in `[from, to)` across `schedules`, as of `now`.
fn ical(
    schedules: &[OnCallScheduleDetail],
    owner: &FeedOwner,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
    now: DateTime<Utc>,
) -> String {
    let events: Vec<Event> = schedules
        .iter()
        .flat_map(|d| {
            shifts_held_by(&d.schedule, &d.layers, &d.overrides, owner.user, from, to).map(|r| {
                Event {
                    uid: format!(
                        "{}-{}-{}@uptimepage",
                        d.schedule.id,
                        owner.user.0,
                        r.start.timestamp()
                    ),
                    starts_at: r.start,
                    ends_at: r.end,
                    summary: format!("On call: {}", d.schedule.name),
                }
            })
        })
        .collect();
    calendar(&format!("On call · {}", owner.org_name), now, &events)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{
        OnCallLayer, OnCallOverride, OnCallParticipant, OnCallSchedule, OrgId, RotationType,
    };
    use uuid::Uuid;

    fn t(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    fn uid(n: u128) -> UserId {
        UserId(Uuid::from_u128(n))
    }

    /// A daily rotation handing off at 09:00 UTC through `people`.
    fn rota(
        id: u128,
        name: &str,
        people: &[u128],
        overrides: Vec<OnCallOverride>,
    ) -> OnCallScheduleDetail {
        OnCallScheduleDetail {
            schedule: OnCallSchedule {
                id: Uuid::from_u128(id),
                name: name.into(),
                timezone: "UTC".into(),
                created_at: t("2026-01-01T00:00:00Z"),
                updated_at: t("2026-01-01T00:00:00Z"),
            },
            layers: vec![OnCallLayer {
                id: Uuid::now_v7(),
                name: None,
                rotation_type: RotationType::Daily,
                rotation_length_secs: 86_400,
                handoff_at: t("2026-09-01T09:00:00Z"),
                layer_order: 0,
                windows: vec![],
                created_at: t("2026-01-01T00:00:00Z"),
                participants: people
                    .iter()
                    .enumerate()
                    .map(|(i, n)| OnCallParticipant {
                        id: Uuid::now_v7(),
                        user_id: uid(*n),
                        position: i as i32,
                    })
                    .collect(),
            }],
            overrides,
        }
    }

    #[test]
    fn upcoming_lists_the_soonest_shifts_across_schedules() {
        let now = t("2026-09-26T12:00:00Z");
        let schedules = [
            rota(1, "Primary", &[1, 2], vec![]),
            rota(2, "Database", &[2, 1], vec![]),
        ];
        let got = upcoming(&schedules, uid(2), now);
        assert_eq!(got.len(), SHIFTS_SHOWN);
        let brief: Vec<_> = got
            .iter()
            .map(|s| (s.schedule.as_str(), s.now, s.starts_at))
            .collect();
        assert_eq!(
            brief[..3],
            [
                ("Primary", true, now),
                ("Database", false, t("2026-09-27T09:00:00Z")),
                ("Primary", false, t("2026-09-28T09:00:00Z")),
            ]
        );
        assert_eq!(got[0].ends_at, Some(t("2026-09-27T09:00:00Z")));
    }

    #[test]
    fn a_rotation_of_one_has_no_end_in_sight() {
        let now = t("2026-09-26T12:00:00Z");
        let got = upcoming(&[rota(1, "Primary", &[1], vec![])], uid(1), now);
        assert_eq!(got.len(), 1);
        assert!(got[0].now);
        assert_eq!(got[0].ends_at, None);
        assert!(upcoming(&[rota(1, "Primary", &[1], vec![])], uid(2), now).is_empty());
    }

    #[test]
    fn the_feed_holds_the_owner_s_shifts_in_the_window() {
        let owner = FeedOwner {
            org: OrgId(Uuid::from_u128(9)),
            user: uid(2),
            org_name: "Acme".into(),
        };
        let cover = OnCallOverride {
            id: Uuid::now_v7(),
            user_id: uid(3),
            starts_at: t("2026-09-26T09:00:00Z"),
            ends_at: t("2026-09-26T21:00:00Z"),
            created_by: None,
            created_at: t("2026-09-01T00:00:00Z"),
        };
        let ics = ical(
            &[rota(7, "Primary", &[1, 2], vec![cover])],
            &owner,
            t("2026-09-25T00:00:00Z"),
            t("2026-09-29T00:00:00Z"),
            t("2026-09-26T12:00:00Z"),
        );
        assert!(ics.contains("X-WR-CALNAME:On call · Acme\r\n"));
        let starts: Vec<&str> = ics
            .lines()
            .filter_map(|l| l.strip_prefix("DTSTART:"))
            .collect();
        assert_eq!(
            starts,
            ["20260925T000000Z", "20260926T210000Z", "20260928T090000Z"]
        );
        assert!(ics.contains("DTEND:20260925T090000Z\r\n"));
        assert!(ics.contains("DTEND:20260927T090000Z\r\n"));
        assert!(ics.contains("DTEND:20260929T000000Z\r\n"));
        assert!(ics.replace("\r\n ", "").contains(&format!(
            "UID:{}-{}-{}@uptimepage\r\n",
            Uuid::from_u128(7),
            uid(2).0,
            t("2026-09-26T21:00:00Z").timestamp()
        )));
        assert!(ics.contains("SUMMARY:On call: Primary\r\n"));
    }

    #[test]
    fn the_feed_window_is_cut_at_utc_midnights() {
        assert_eq!(
            feed_window(t("2026-09-26T12:34:56.789Z")),
            (t("2026-08-27T00:00:00Z"), t("2026-12-25T00:00:00Z"))
        );
    }

    #[test]
    fn partial_offers_a_link_until_there_is_one() {
        let html = ShiftsPartial {
            shifts: vec![],
            feed_url: None,
        }
        .render()
        .unwrap();
        assert!(html.contains("# no shifts for you in the next 90 days"));
        assert!(html.contains("make calendar link"));
        assert!(!html.contains("data-copy"));
    }

    #[test]
    fn partial_lists_shifts_and_the_link() {
        let html = ShiftsPartial {
            shifts: vec![
                MyShift {
                    schedule: "Primary".into(),
                    now: true,
                    starts_at: t("2026-09-26T12:00:00Z"),
                    ends_at: Some(t("2026-09-27T09:00:00Z")),
                },
                MyShift {
                    schedule: "Database".into(),
                    now: false,
                    starts_at: t("2026-10-01T09:00:00Z"),
                    ends_at: None,
                },
            ],
            feed_url: Some("https://app.example.com/ical/tok.ics".into()),
        }
        .render()
        .unwrap();
        assert!(html.contains(
            r#"now
          until <time data-tz="at" datetime="2026-09-27T09:00:00Z">"#
        ));
        assert!(html.contains(r#"<time data-tz="at" datetime="2026-10-01T09:00:00Z">"#));
        assert!(html.contains("with no handoff in the next 90 days"));
        assert!(html.contains("https://app.example.com/ical/tok.ics</code>"));
        assert!(html.contains(r##"data-copy="#on-call-feed-url""##));
        assert!(html.contains("new link"));
        assert!(!html.contains("make calendar link"));
    }
}
