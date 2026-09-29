//! Pushover emergency pages: polling their receipts for who acknowledged,
//! offering an unlinked acknowledger a link, and cancelling the ones nothing is
//! left to answer.

use chrono::Utc;
use futures::stream::{FuturesUnordered, StreamExt};
use tokio::time::Instant;
use uuid::Uuid;

use crate::app_accounts::{LinkOffer, identify, offer_link};
use crate::domain::{ChannelConfig, ExternalId, Linked, LinkedApp, OrgId};
use crate::error::Result;
use crate::notifier::pushover::PushoverReceipts;
use crate::security::app_link::external_id;
use crate::storage::{Acknowledged, Actor, EmergencyAck, LifecycleOutcome};

use super::rules::{offers_pushover_link, pushover_acknowledger};
use super::{SWEEP_CONCURRENCY, Worker};

impl Worker {
    /// Build a Pushover receipt client from the channel's stored application
    /// token. `None` if the channel is gone or not a Pushover channel.
    /// `Err` means the lookup failed and the caller must not conclude anything
    /// about the channel; `Ok(None)` means it is genuinely gone or is not a
    /// Pushover channel, and its receipt can never be cancelled or read.
    async fn pushover_receipts(
        &self,
        org: OrgId,
        channel_id: Uuid,
    ) -> Result<Option<PushoverReceipts>> {
        Ok(match self.channels.get(org, channel_id).await? {
            Some(channel) => match &channel.config {
                ChannelConfig::Pushover(c) => {
                    Some(PushoverReceipts::new(self.http.clone(), c.token.clone()))
                }
                _ => None,
            },
            None => None,
        })
    }

    /// Stop every emergency page still repeating for an incident. A receipt is
    /// retired only once its cancel lands, or a failure would leave the page
    /// sounding with nothing left to retry it; Pushover's `expire` bounds one
    /// that never lands.
    pub(super) async fn cancel_emergency(&self, org: OrgId, incident_id: Uuid) {
        let acks = match self.ops.emergency_acks_for_incident(org, incident_id).await {
            Ok(a) => a,
            Err(err) => {
                tracing::warn!(error = %err, "emergency ack lookup failed");
                return;
            }
        };
        for ack in acks {
            let client = match self.pushover_receipts(org, ack.channel_id).await {
                // Nothing can cancel it — retire the receipt.
                Ok(None) => {
                    self.clear_receipt(&ack).await;
                    continue;
                }
                Ok(Some(client)) => client,
                // Leave it for the next sweep: retiring a receipt we could not
                // even look up would strand a page that keeps sounding.
                Err(err) => {
                    tracing::warn!(error = %err, "pushover channel lookup failed");
                    continue;
                }
            };
            if let Err(err) = client.cancel(&ack.receipt).await {
                tracing::warn!(incident_id = %incident_id, error = %err, "pushover emergency cancel failed");
                continue;
            }
            self.clear_receipt(&ack).await;
        }
    }

    /// Takes a receipt out of the poll set. A failure leaves it there for the
    /// next sweep to settle.
    async fn clear_receipt(&self, ack: &EmergencyAck) {
        if let Err(err) = self.ops.clear_receipt(ack.org, ack.id).await {
            tracing::warn!(
                org_id = %ack.org.0,
                incident_id = %ack.incident_id,
                notification_id = %ack.id,
                error = %err,
                "clearing emergency receipt failed"
            );
        }
    }

    /// Records when the page was taken and takes its receipt out of the poll
    /// set. A failure leaves it there for the next sweep, unless the cancel
    /// that follows an acknowledgement retires it first, without the time.
    async fn mark_acked(&self, ack: &EmergencyAck) {
        if let Err(err) = self.ops.mark_acked(ack.org, ack.id, Utc::now()).await {
            tracing::warn!(
                org_id = %ack.org.0,
                incident_id = %ack.incident_id,
                notification_id = %ack.id,
                error = %err,
                "marking emergency receipt acknowledged failed"
            );
        }
    }

    /// Nothing is left for this page to acknowledge: its incident was taken by
    /// somebody, resolved, or removed, its outage ended and another has since
    /// started, or its monitor is no longer watched. One incident read serves
    /// all three. Any failed lookup reads as still paging, so an error never
    /// cancels a live page.
    pub(super) async fn page_is_spent(&self, ack: &EmergencyAck) -> bool {
        let incident = match self.ops.get(ack.org, ack.incident_id).await {
            Ok(Some(incident)) => incident,
            Ok(None) => return true,
            Err(_) => return false,
        };
        if incident.state != crate::domain::IncidentState::Triggered {
            return true;
        }
        if matches!(
            self.ops.generation(ack.org, ack.incident_id).await,
            Ok(Some(current)) if current != ack.generation
        ) {
            return true;
        }
        let Some(target_id) = incident.target_id else {
            return false;
        };
        matches!(self.targets.get(ack.org, target_id).await,
            Ok(Some(t)) if !t.enabled || t.plan_hold_at.is_some())
    }

    /// Poll outstanding emergency receipts: record acknowledgement on the
    /// timeline, drop the receipt once acked or expired so it leaves the poll
    /// set.
    pub(super) async fn poll_acks(&self) {
        let limit = self.cfg.max_pages_per_tick.max(1) as usize;
        let due = match self.ops.due_emergency_acks(limit).await {
            Ok(d) => d,
            Err(err) => {
                tracing::warn!(error = %err, "emergency ack poll scan failed");
                return;
            }
        };
        let budget = self.sweep_budget();
        let start = Instant::now();
        let mut it = due.into_iter();
        let mut futs = FuturesUnordered::new();
        for a in it.by_ref().take(SWEEP_CONCURRENCY) {
            futs.push(self.poll_one_ack(a));
        }
        while futs.next().await.is_some() {
            if start.elapsed() < budget
                && let Some(a) = it.next()
            {
                futs.push(self.poll_one_ack(a));
            }
        }
    }

    /// Offer the Pushover account that acknowledged without a name a link to
    /// whoever holds it, at most once per cooldown. Best effort and off the
    /// sweep: the acknowledgement already landed, and a slow Pushover must not
    /// hold a poll slot.
    async fn offer_pushover_link(
        &self,
        client: PushoverReceipts,
        user_key: String,
        sender: ExternalId,
        device: Option<String>,
    ) {
        let Some(LinkOffer { url, code_hash }) = offer_link(
            self.linked_apps.as_ref(),
            &self.base_url,
            LinkedApp::Pushover,
            sender,
            device.as_deref(),
            Utc::now(),
        )
        .await
        else {
            return;
        };
        let linked_apps = self.linked_apps.clone();
        tokio::spawn(async move {
            if let Err(err) = client.offer_link(&user_key, &url).await {
                tracing::warn!(error = %err, "pushover link offer failed");
                // Unsent, so the next acknowledgement may try again. A timeout
                // after Pushover accepted it withdraws a link that still
                // arrives; the next eligible acknowledgement offers another.
                if let Err(err) = linked_apps
                    .withdraw_offer(LinkedApp::Pushover, &code_hash)
                    .await
                {
                    tracing::warn!(error = %err, "pushover link offer withdrawal failed");
                }
            }
        });
    }

    async fn poll_one_ack(&self, ack: EmergencyAck) {
        let client = match self.pushover_receipts(ack.org, ack.channel_id).await {
            // Nothing can cancel or read it — retire the receipt.
            Ok(None) => {
                self.clear_receipt(&ack).await;
                return;
            }
            Ok(Some(client)) => client,
            Err(err) => {
                tracing::warn!(error = %err, "pushover channel lookup failed");
                return;
            }
        };
        let state = match client.poll(&ack.receipt).await {
            Ok(s) => s,
            Err(err) => {
                tracing::warn!(error = %err, "pushover receipt poll failed");
                return;
            }
        };
        if state.acknowledged {
            let sender = state
                .acknowledged_by
                .as_deref()
                .map(|key| external_id(&self.app_link_secret, key));
            let linked = match sender {
                Some(sender) => {
                    identify(
                        self.linked_apps.as_ref(),
                        ack.org,
                        LinkedApp::Pushover,
                        sender,
                    )
                    .await
                }
                None => Linked::Unknown,
            };
            let actor = pushover_acknowledger(sender, linked);
            // Only a receipt with nobody to name needs to say where it came from.
            let note = matches!(actor, Actor::Link).then(|| "Acknowledged in Pushover".to_string());
            // Pushover stops its own retries on an ack, but until this landed
            // nothing here knew the page was taken, so renotify kept paging.
            match self
                .ops
                .acknowledge(
                    ack.org,
                    ack.incident_id,
                    actor,
                    note,
                    // A reopen tries to cancel this receipt, but the signal
                    // can be dropped under load and the cancel can fail, so
                    // the episode is pinned rather than assumed.
                    Some(ack.generation),
                )
                .await
            {
                Ok(Acknowledged {
                    outcome: LifecycleOutcome::Updated(_),
                    listed,
                }) => {
                    // Marked first, so the sweep below skips this row.
                    self.mark_acked(&ack).await;
                    self.cancel_emergency(ack.org, ack.incident_id).await;
                    if offers_pushover_link(listed, linked)
                        && let (Some(key), Some(sender)) = (state.acknowledged_by, sender)
                    {
                        self.offer_pushover_link(client, key, sender, state.acknowledged_by_device)
                            .await;
                    }
                }
                // Terminal: no later poll does better.
                Ok(Acknowledged {
                    outcome:
                        LifecycleOutcome::NotFound
                        | LifecycleOutcome::IllegalTransition(_)
                        | LifecycleOutcome::Stale,
                    ..
                }) => {
                    self.mark_acked(&ack).await;
                }
                // Unmarked, so the next sweep retries; marking it here would
                // lose the acknowledgement for good.
                Err(err) => {
                    tracing::warn!(error = %err, "recording emergency ack failed");
                }
            }
        } else if state.expired {
            self.clear_receipt(&ack).await;
        } else if self.page_is_spent(&ack).await {
            // Nothing should still be sounding for it. Retired only once the
            // cancel lands, so a failure comes back on the next sweep.
            if let Err(err) = client.cancel(&ack.receipt).await {
                tracing::warn!(incident_id = %ack.incident_id, error = %err, "pushover emergency cancel failed");
                return;
            }
            self.clear_receipt(&ack).await;
        }
    }
}
