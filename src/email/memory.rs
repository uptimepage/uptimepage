use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

use async_trait::async_trait;

use super::trait_def::{
    BatchFailure, BatchResult, EmailError, EmailResult, EmailSender, MessageId, PreparedBatch,
    SendClass, TransactionalEmail, check_batch_size,
};

#[derive(Default, Clone)]
pub struct InMemoryEmailSender {
    inner: Arc<Mutex<Captured>>,
}

#[derive(Default)]
struct Captured {
    sent: Vec<TransactionalEmail>,
    /// Prepared batches by key, until they are sent.
    prepared: HashMap<String, Vec<TransactionalEmail>>,
    /// What each sent key answered, so a resend of the same key is answered
    /// again without capturing twice, as the provider does.
    answered: HashMap<String, Vec<MessageId>>,
}

impl InMemoryEmailSender {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, Captured> {
        self.inner.lock().expect("memory sender mutex poisoned")
    }

    /// Snapshot of every email send() has accepted. Cheap clone of the inner
    /// vector; tests typically read once at assertion time.
    pub fn sent(&self) -> Vec<TransactionalEmail> {
        self.lock().sent.clone()
    }

    pub fn len(&self) -> usize {
        self.lock().sent.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn clear(&self) {
        *self.lock() = Captured::default();
    }

    fn capture(captured: &mut Captured, email: TransactionalEmail) -> MessageId {
        let id = MessageId(format!("memory-{}", captured.sent.len()));
        captured.sent.push(email);
        id
    }
}

#[async_trait]
impl EmailSender for InMemoryEmailSender {
    async fn send(&self, email: TransactionalEmail, _class: SendClass) -> EmailResult<MessageId> {
        Ok(Self::capture(&mut self.lock(), email))
    }

    fn prepare_batch(
        &self,
        emails: Vec<TransactionalEmail>,
        key: String,
    ) -> EmailResult<PreparedBatch> {
        check_batch_size(emails.len())?;
        let len = emails.len();
        let body = key.clone().into_bytes().into();
        self.lock().prepared.insert(key.clone(), emails);
        Ok(PreparedBatch::from_parts(key, body, len))
    }

    async fn send_prepared(&self, batch: PreparedBatch) -> BatchResult {
        let mut captured = self.lock();
        if let Some(ids) = captured.answered.get(batch.key()) {
            return Ok(ids.iter().cloned().map(Ok).collect());
        }
        let Some(emails) = captured.prepared.remove(batch.key()) else {
            drop(captured);
            return Err(BatchFailure {
                error: EmailError::Transport("batch was never prepared here".into()),
                batch,
            });
        };
        let ids: Vec<MessageId> = emails
            .into_iter()
            .map(|e| Self::capture(&mut captured, e))
            .collect();
        captured
            .answered
            .insert(batch.key().to_string(), ids.clone());
        Ok(ids.into_iter().map(Ok).collect())
    }
}
