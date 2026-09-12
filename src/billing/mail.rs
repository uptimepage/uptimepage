//! Subscription mail to the account owner. Sent after the change commits and
//! never allowed to fail it: a mail that did not go out is logged, the state
//! it describes is already true.

use sqlx::PgPool;

use crate::domain::{AccountId, UserId};
use crate::email::{EmailAddress, EmailTemplate, TransactionalEmail};
use crate::notifier::EmailDelivery;

pub struct Mailer {
    pub delivery: EmailDelivery,
    /// App origin the links are built on. Empty leaves the links relative.
    pub public_base_url: String,
}

impl Mailer {
    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.public_base_url.trim_end_matches('/'))
    }

    /// Where the owner chooses what a smaller plan keeps.
    pub fn keep_url(&self) -> String {
        self.url("/settings/usage")
    }

    /// Where the owner updates the card on file.
    pub fn fix_url(&self) -> String {
        self.url("/settings/billing/payment-method")
    }

    pub async fn send(
        &self,
        pool: &PgPool,
        account: AccountId,
        owner: Option<UserId>,
        template: EmailTemplate,
    ) {
        let Some(owner) = owner else {
            return;
        };
        let recipient: Option<(String,)> =
            match sqlx::query_as("SELECT email FROM users WHERE id = $1 AND deleted_at IS NULL")
                .bind(owner.0)
                .fetch_optional(pool)
                .await
            {
                Ok(row) => row,
                Err(err) => {
                    tracing::warn!(account = %account, error = %err, "billing mail: owner lookup");
                    return;
                }
            };
        let Some((email,)) = recipient else {
            return;
        };
        let outgoing = TransactionalEmail {
            from: EmailAddress::new(
                self.delivery.from_address.clone(),
                self.delivery.from_name.clone(),
            ),
            to: EmailAddress::new(email.clone(), email),
            template,
        };
        if let Err(err) = self.delivery.sender.send(outgoing).await {
            tracing::warn!(account = %account, error = %err, "billing mail not sent");
        }
    }
}
