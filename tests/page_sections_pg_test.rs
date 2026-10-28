//! A monitor group shown on a status page as a live section, against Postgres:
//! rows follow the group, the cap leaves members off, leaving revokes detail
//! links, reorder keeps a section together, and the page groups by key.

use crate::common;

use std::time::Duration;

use sqlx::PgPool;
use url::Url;
use uuid::Uuid;

use uptimepage::domain::{
    CheckSpec, ExpectedStatus, MonitorShareId, NewMonitorGroup, NewMonitorShare, NewStatusPage,
    NewStatusPageComponent, NewTarget, OrgId, StatusPageComponentUpdate, StatusPageId,
    TargetUpdate, WriteCaps, WriteSource,
};
use uptimepage::error::{AppError, codes};
use uptimepage::storage::page_sections;
use uptimepage::storage::{
    CreateShareOutcome, MonitorGroupStore, MonitorShareStore, PgMonitorGroupStore,
    PgMonitorShareStore, PgStatusPageStore, PostgresTargetStore, StatusPageStore, TargetStore,
    create_org_with_owner,
};

struct Fixture {
    pool: PgPool,
    org: OrgId,
    targets: PostgresTargetStore,
    groups: PgMonitorGroupStore,
    pages: PgStatusPageStore,
    page: StatusPageId,
}

impl Fixture {
    async fn new() -> Option<Self> {
        let pool = common::pg_pool_from_env().await?;
        let user = common::make_user(&pool, "sec").await;
        let org = create_org_with_owner(&pool, user, &common::unique_slug("sec"), "Co")
            .await
            .unwrap()
            .unwrap()
            .id;
        let pages = PgStatusPageStore::new(pool.clone());
        let page = pages
            .create(
                org,
                NewStatusPage {
                    access: None,
                    slug: common::unique_slug("secpage"),
                    name: "Status".into(),
                    enabled: true,
                },
                WriteSource::Ui,
                i64::MAX,
                None,
            )
            .await
            .unwrap()
            .unwrap()
            .id;
        Some(Self {
            targets: PostgresTargetStore::from_pool(pool.clone(), None),
            groups: PgMonitorGroupStore::new(pool.clone()),
            pages,
            pool,
            org,
            page,
        })
    }

    async fn group(&self, name: &str) -> Uuid {
        self.groups
            .create(
                self.org,
                NewMonitorGroup::named(name),
                WriteSource::Ui,
                None,
                i64::MAX,
            )
            .await
            .unwrap()
            .id
    }

    async fn monitor(&self, name: &str, group: Option<Uuid>) -> Uuid {
        let new = NewTarget {
            name: name.into(),
            check: CheckSpec::Http(common::default_http_check(
                Url::parse("https://example.com/").unwrap(),
                ExpectedStatus::Exact(200),
            )),
            interval: Duration::from_secs(60),
            enabled: true,
            tags: vec![],
            alerts: Default::default(),
            region_policy: Default::default(),
            alert_confirmations: 1,
            notify_recovery: true,
            renotify_interval_secs: 3600,
            recovery_period_secs: None,
            group_name: None,
            group_id: group,
            owner_user_id: None,
            regions: None,
        };
        self.targets
            .create(self.org, new, WriteSource::Ui, None, WriteCaps::UNLIMITED)
            .await
            .unwrap()
            .id
    }

    async fn by_hand(&self, target: Uuid, public_name: Option<&str>) {
        self.pages
            .add_component(
                self.org,
                self.page,
                NewStatusPageComponent {
                    target_id: target,
                    public_name: public_name.map(str::to_owned),
                    public_description: None,
                    public_group: None,
                    sort_order: 0,
                    detail_link_enabled: false,
                },
                i64::MAX,
                None,
            )
            .await
            .unwrap();
    }

    async fn rows(&self) -> Vec<(Uuid, Option<Uuid>, Option<String>)> {
        self.pages
            .list_components(self.org, self.page)
            .await
            .unwrap()
            .into_iter()
            .map(|c| (c.target_id, c.section_id, c.public_name))
            .collect()
    }

    async fn move_to(&self, target: Uuid, group: Option<Uuid>) {
        self.targets
            .update(
                self.org,
                target,
                TargetUpdate {
                    group_id: Some(group),
                    ..Default::default()
                },
                Some(WriteSource::Ui),
                None,
                WriteCaps::UNLIMITED,
            )
            .await
            .unwrap()
            .expect("monitor");
    }
}

fn on_page(rows: &[(Uuid, Option<Uuid>, Option<String>)], target: Uuid) -> bool {
    rows.iter().any(|(t, _, _)| *t == target)
}

#[tokio::test]
#[ignore = "needs DATABASE_URL"]
async fn a_section_mirrors_its_group_on_the_page() {
    let Some(f) = Fixture::new().await else {
        return;
    };
    let dns = f.group("example.com DNS").await;
    let ns = f.monitor("NS", Some(dns)).await;
    let a = f.monitor("A", Some(dns)).await;
    let mx = f.monitor("MX", Some(dns)).await;
    let web = f.monitor("web", None).await;
    f.by_hand(mx, Some("Mail")).await;
    f.by_hand(web, None).await;

    let (section, change) = page_sections::add(
        &f.pool,
        f.org,
        f.page.0,
        dns,
        Some("DNS".into()),
        i64::MAX,
        None,
    )
    .await
    .unwrap();
    assert!(change.pages.contains(&f.page.0));
    let rows = f.rows().await;
    for member in [ns, a, mx] {
        let row = rows.iter().find(|r| r.0 == member).expect("member on page");
        assert_eq!(row.1, Some(section));
    }
    let mx_row = rows.iter().find(|r| r.0 == mx).unwrap();
    assert_eq!(
        mx_row.2.as_deref(),
        Some("Mail"),
        "a hand-added row keeps its settings"
    );
    assert_eq!(rows.iter().find(|r| r.0 == web).unwrap().1, None);

    let txt = f.monitor("TXT", Some(dns)).await;
    assert!(on_page(&f.rows().await, txt), "a new member joins the page");

    f.move_to(a, None).await;
    assert!(
        !on_page(&f.rows().await, a),
        "a member that leaves drops off"
    );

    f.targets
        .set_group(
            f.org,
            &[web],
            Some("example.com DNS"),
            None,
            WriteCaps::UNLIMITED,
        )
        .await
        .unwrap();
    let rows = f.rows().await;
    assert_eq!(
        rows.iter().find(|r| r.0 == web).unwrap().1,
        Some(section),
        "a monitor on the page by hand moves into the section"
    );

    let refused = f
        .pages
        .remove_component(f.org, f.page, ns, None)
        .await
        .unwrap_err();
    assert!(
        matches!(
            refused,
            AppError::Conflict {
                code: codes::SECTION_MEMBER,
                ..
            }
        ),
        "{refused:?}"
    );

    let listed = page_sections::list(&f.pool, f.org, f.page.0).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].members.len(), 4);
    assert_eq!(listed[0].heading.as_deref(), Some("DNS"));

    page_sections::remove(&f.pool, f.org, f.page.0, section, None)
        .await
        .unwrap();
    let rows = f.rows().await;
    assert!(
        rows.is_empty(),
        "every member leaves with the section: {rows:?}"
    );
    let audits: Vec<String> = sqlx::query_scalar(
        "SELECT action FROM org_audit_log WHERE org_id = $1 AND action LIKE 'status_page.section%' \
         ORDER BY occurred_at, id",
    )
    .bind(f.org.0)
    .fetch_all(&f.pool)
    .await
    .unwrap();
    assert_eq!(
        audits,
        ["status_page.section_added", "status_page.section_removed"]
    );
}

#[tokio::test]
#[ignore = "needs DATABASE_URL"]
async fn deleting_the_group_takes_its_rows_off_every_page() {
    let Some(f) = Fixture::new().await else {
        return;
    };
    let dns = f.group("DNS").await;
    let ns = f.monitor("NS", Some(dns)).await;
    let other = f.monitor("other", None).await;
    f.by_hand(other, None).await;
    page_sections::add(&f.pool, f.org, f.page.0, dns, None, i64::MAX, None)
        .await
        .unwrap();
    assert!(on_page(&f.rows().await, ns));

    let deleted = f.groups.delete(f.org, dns, None).await.unwrap().unwrap();
    assert_eq!(deleted.pages, vec![f.page.0]);
    let rows = f.rows().await;
    assert!(!on_page(&rows, ns));
    assert!(on_page(&rows, other), "rows outside the section stay");
    assert!(
        page_sections::list(&f.pool, f.org, f.page.0)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
#[ignore = "needs DATABASE_URL"]
async fn members_past_the_public_component_cap_stay_off() {
    let Some(f) = Fixture::new().await else {
        return;
    };
    let dns = f.group("DNS").await;
    for name in ["A", "B", "C"] {
        f.monitor(name, Some(dns)).await;
    }
    let (_, change) = page_sections::add(&f.pool, f.org, f.page.0, dns, None, 2, None)
        .await
        .unwrap();
    assert_eq!(change.left_off, 1);
    let listed = page_sections::list(&f.pool, f.org, f.page.0).await.unwrap();
    assert_eq!(listed[0].members.len(), 2);
    assert_eq!(listed[0].left_off, 1);

    let later = f.monitor("D", None).await;
    f.targets
        .update(
            f.org,
            later,
            TargetUpdate {
                group_id: Some(Some(dns)),
                ..Default::default()
            },
            Some(WriteSource::Ui),
            None,
            WriteCaps {
                public_components: 2,
                ..WriteCaps::UNLIMITED
            },
        )
        .await
        .unwrap()
        .unwrap();
    assert!(
        !on_page(&f.rows().await, later),
        "a joining member past the cap stays off, the move itself goes through"
    );
}

#[tokio::test]
#[ignore = "needs DATABASE_URL"]
async fn leaving_the_group_revokes_the_detail_link() {
    let Some(f) = Fixture::new().await else {
        return;
    };
    let dns = f.group("DNS").await;
    let ns = f.monitor("NS", Some(dns)).await;
    page_sections::add(&f.pool, f.org, f.page.0, dns, None, i64::MAX, None)
        .await
        .unwrap();
    let shares = PgMonitorShareStore::new(f.pool.clone(), Some(common::test_cipher()));
    let CreateShareOutcome::Created(created) = shares
        .create(
            f.org,
            ns,
            NewMonitorShare {
                label: None,
                expires_at: None,
            },
            None,
            None,
            None,
        )
        .await
        .unwrap()
    else {
        panic!("share");
    };
    let share: MonitorShareId = created.share.id;
    assert!(
        f.pages
            .attach_share(f.org, f.page, ns, None, share)
            .await
            .unwrap()
    );
    f.pages
        .update_component(
            f.org,
            f.page,
            ns,
            StatusPageComponentUpdate {
                detail_link_enabled: Some(true),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    f.move_to(ns, None).await;
    let revoked: Option<chrono::DateTime<chrono::Utc>> =
        sqlx::query_scalar("SELECT revoked_at FROM monitor_shares WHERE id = $1")
            .bind(share.0)
            .fetch_one(&f.pool)
            .await
            .unwrap();
    assert!(
        revoked.is_some(),
        "the departed member's link stops working"
    );
}

#[tokio::test]
#[ignore = "needs DATABASE_URL"]
async fn reorder_keeps_a_section_together() {
    let Some(f) = Fixture::new().await else {
        return;
    };
    let dns = f.group("DNS").await;
    let ns = f.monitor("NS", Some(dns)).await;
    let a = f.monitor("A", Some(dns)).await;
    let web = f.monitor("web", None).await;
    f.by_hand(web, None).await;
    page_sections::add(&f.pool, f.org, f.page.0, dns, None, i64::MAX, None)
        .await
        .unwrap();
    f.pages
        .reorder_components(f.org, f.page, &[ns, web, a])
        .await
        .unwrap();
    let order: Vec<Uuid> = f.rows().await.into_iter().map(|r| r.0).collect();
    assert_eq!(order, [ns, a, web], "the section's members stay adjacent");
}

#[tokio::test]
#[ignore = "requires DATABASE_URL + CLICKHOUSE_URL"]
async fn the_page_groups_rows_by_section_not_by_heading_text() {
    let Some(f) = Fixture::new().await else {
        return;
    };
    let Some(ch) = common::ch_client_from_env().await else {
        return;
    };
    let one = f.group("one").await;
    let two = f.group("two").await;
    let a = f.monitor("a", Some(one)).await;
    let b = f.monitor("b", Some(two)).await;
    let c = f.monitor("c", None).await;
    for g in [one, two] {
        page_sections::add(
            &f.pool,
            f.org,
            f.page.0,
            g,
            Some("Production".into()),
            i64::MAX,
            None,
        )
        .await
        .unwrap();
    }
    f.by_hand(c, None).await;
    f.pages
        .update_component(
            f.org,
            f.page,
            c,
            StatusPageComponentUpdate {
                public_group: Some(Some("Production".into())),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    f.pages
        .reorder_components(f.org, f.page, &[a, b, c])
        .await
        .unwrap();

    let agg = uptimepage::public_status::OrgAggregator::new(
        f.pool.clone(),
        ch,
        uptimepage::public_status::AggregatorConfig::default(),
        None,
    );
    let (page, ..) = agg.build(f.page, f.org).await.unwrap();
    let groups: Vec<(Option<String>, usize, bool)> = page
        .groups
        .iter()
        .map(|g| (g.name.clone(), g.components.len(), g.status.is_some()))
        .collect();
    assert_eq!(
        groups,
        [
            (Some("Production".into()), 1, true),
            (Some("Production".into()), 1, true),
            (Some("Production".into()), 1, false),
        ]
    );
    let json = serde_json::to_value(&page.groups).unwrap();
    assert!(json[0].get("status").is_some());
    assert!(
        json[2].get("status").is_none(),
        "a hand-made heading carries no status"
    );
}

#[tokio::test]
#[ignore = "needs DATABASE_URL"]
async fn racing_moves_creates_and_section_changes_settle_consistently() {
    let Some(f) = Fixture::new().await else {
        return;
    };
    let one = f.group("one").await;
    let two = f.group("two").await;
    let mut monitors = Vec::new();
    for i in 0..6 {
        let g = if i % 2 == 0 { Some(one) } else { Some(two) };
        monitors.push(f.monitor(&format!("m{i}"), g).await);
    }
    page_sections::add(&f.pool, f.org, f.page.0, one, None, i64::MAX, None)
        .await
        .unwrap();
    let (two_section, _) = page_sections::add(&f.pool, f.org, f.page.0, two, None, i64::MAX, None)
        .await
        .unwrap();

    let groups = [Some(one), Some(two), None];
    let moves = monitors.iter().enumerate().map(|(i, m)| {
        let f = &f;
        async move {
            for round in 0..4 {
                f.move_to(*m, groups[(i + round) % 3]).await;
            }
        }
    });
    let creates = async {
        for i in 0..3 {
            f.monitor(&format!("new{i}"), Some(two)).await;
        }
    };
    let sections = async {
        page_sections::remove(&f.pool, f.org, f.page.0, two_section, None)
            .await
            .unwrap();
        page_sections::add(&f.pool, f.org, f.page.0, two, None, i64::MAX, None)
            .await
            .unwrap();
    };
    let bulk = async {
        f.targets
            .set_group(
                f.org,
                &monitors[..2],
                Some("one"),
                None,
                WriteCaps::UNLIMITED,
            )
            .await
            .unwrap();
    };
    tokio::join!(futures::future::join_all(moves), creates, sections, bulk);

    let on_page: std::collections::BTreeSet<Uuid> =
        f.rows().await.into_iter().map(|r| r.0).collect();
    let shown: std::collections::BTreeSet<Uuid> = sqlx::query_scalar(
        "SELECT t.id FROM targets t JOIN status_page_sections s \
           ON s.group_id = t.group_id AND s.org_id = t.org_id \
         WHERE t.org_id = $1 AND s.status_page_id = $2",
    )
    .bind(f.org.0)
    .bind(f.page.0)
    .fetch_all(&f.pool)
    .await
    .unwrap()
    .into_iter()
    .collect();
    assert_eq!(
        on_page, shown,
        "rows are exactly the members of shown groups"
    );
}
