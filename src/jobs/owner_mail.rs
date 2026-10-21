//! What every mail to a monitor's owner shares: who gets it, how it is
//! addressed, and when its stamp failures end a pass.

use uuid::Uuid;

use crate::email::{EmailAddress, EmailTemplate, TransactionalEmail};
use crate::notifier::EmailDelivery;

/// The monitor's own owner first; the org's first owner only when the monitor
/// names nobody, so exactly one person is told. Joins on `targets t`; a monitor
/// whose org has no reachable owner gets a NULL [`RECIPIENT`].
pub(crate) const RECIPIENT_JOINS: &str = "\
    LEFT JOIN users mon_owner \
           ON mon_owner.id = t.owner_user_id AND mon_owner.deleted_at IS NULL \
    LEFT JOIN LATERAL ( \
         SELECT u.email FROM memberships m \
           JOIN users u ON u.id = m.user_id \
          WHERE m.org_id = t.org_id AND m.role = 'owner' AND u.deleted_at IS NULL \
          ORDER BY u.email LIMIT 1 \
    ) org_owner ON true";
pub(crate) const RECIPIENT: &str = "COALESCE(mon_owner.email, org_owner.email)";

/// Consecutive stamp failures that end a pass. The stamp is the eligibility
/// filter, so a store that refuses writes while accepting reads would re-send
/// the whole batch every pass.
pub const STAMP_FAILURE_LIMIT: u32 = 3;

pub struct OwnerMail {
    pub email: EmailDelivery,
    /// App base URL. Empty disables the deep link rather than mailing a
    /// half-formed one.
    pub public_base_url: String,
    pub docs_url: Option<String>,
}

impl OwnerMail {
    pub(crate) fn monitor_url(&self, target_id: Uuid) -> Option<String> {
        let base = self.public_base_url.trim_end_matches('/');
        (!base.is_empty()).then(|| format!("{base}/targets/{target_id}"))
    }

    pub(crate) fn to(&self, recipient: &str, template: EmailTemplate) -> TransactionalEmail {
        TransactionalEmail {
            from: EmailAddress::new(
                self.email.from_address.clone(),
                self.email.from_name.clone(),
            ),
            to: EmailAddress::new(recipient, recipient),
            template,
        }
    }
}

#[derive(Default)]
pub(crate) struct StampFailures(u32);

impl StampFailures {
    /// Records the stamp after an accepted send and says whether the pass
    /// must stop. A failed stamp costs a duplicate next pass.
    pub(crate) fn should_stop<T>(
        &mut self,
        stamped: Result<T, sqlx::Error>,
        job: &str,
        sent: u64,
    ) -> bool {
        let Err(err) = stamped else {
            self.0 = 0;
            return false;
        };
        self.0 += 1;
        tracing::warn!(error = %err, "{job} sent but not stamped");
        if self.0 < STAMP_FAILURE_LIMIT {
            return false;
        }
        tracing::error!(
            sent,
            "{job} stopped: stamps are failing, every send repeats next pass"
        );
        true
    }
}
