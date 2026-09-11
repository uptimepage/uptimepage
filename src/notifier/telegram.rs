use std::sync::Arc;

use async_trait::async_trait;
use serde::Serialize;
use url::Url;

use crate::error::{AppError, Result};
use crate::http_outbound::{OutboundHttpClient, post_json};
use crate::notifier::event::IncidentNotice;
use crate::notifier::{ChatMigration, Notifier, json_int_field};
use crate::telegram::TelegramSendBudget;

/// Telegram Bot API sender. The bot token is embedded in the fixed
/// `api.telegram.org` endpoint path; `chat_id` is sent in the body.
/// `budget` is set only for the central bot (shared across orgs); BYO bots
/// have their own per-customer budget and go unmetered.
pub struct TelegramNotifier {
    client: OutboundHttpClient,
    send_url: Url,
    chat_id: String,
    budget: Option<Arc<TelegramSendBudget>>,
    /// Set whether or not the second send lands: the old id is dead either way.
    moved_to: parking_lot::Mutex<Option<String>>,
}

fn migrated_chat_id(error: &str) -> Option<i64> {
    json_int_field(error, "migrate_to_chat_id")
}

#[derive(Serialize)]
struct SendMessage<'a> {
    chat_id: &'a str,
    text: &'a str,
}

impl TelegramNotifier {
    pub fn new(client: OutboundHttpClient, bot_token: &str, chat_id: String) -> Result<Self> {
        // Host is fixed; only the token (already validated non-empty on
        // channel create) varies. Parsing guards against a token with URL
        // metacharacters reaching the path.
        let send_url = format!("https://api.telegram.org/bot{bot_token}/sendMessage")
            .parse::<Url>()
            .map_err(|e| {
                AppError::bad_request(
                    crate::api::codes::INVALID_CONFIG,
                    format!("telegram bot_token is not URL-safe: {e}"),
                )
            })?;
        Ok(Self::at(client, send_url, chat_id))
    }

    fn at(client: OutboundHttpClient, send_url: Url, chat_id: String) -> Self {
        Self {
            client,
            send_url,
            chat_id,
            budget: None,
            moved_to: parking_lot::Mutex::new(None),
        }
    }

    pub fn with_budget(mut self, budget: Arc<TelegramSendBudget>) -> Self {
        self.budget = Some(budget);
        self
    }
}

impl TelegramNotifier {
    async fn send(&self, chat_id: &str, text: &str) -> Result<()> {
        if let Some(budget) = &self.budget {
            let chat = chat_id.parse::<i64>().unwrap_or_default();
            // The `"retry_after":N` fragment rides the same engine path as a
            // vendor 429 hint, scheduling the retry instead of burning the
            // ceiling — the send never reached Telegram.
            budget.acquire(chat).await.map_err(|d| {
                AppError::Other(anyhow::anyhow!(
                    "telegram send deferred by the local bot budget: {{\"retry_after\":{}}}",
                    d.retry_after_secs
                ))
            })?;
        }
        post_json(&self.client, &self.send_url, &SendMessage { chat_id, text }).await
    }
}

#[async_trait]
impl Notifier for TelegramNotifier {
    async fn notify_incident(&self, notice: &IncidentNotice) -> Result<()> {
        let text = notice.plain_text();
        let err = match self.send(&self.chat_id, &text).await {
            Ok(()) => return Ok(()),
            Err(err) => err,
        };
        // Resend now: a retry against the stored id would hit the same wall.
        let Some(to) = migrated_chat_id(&err.to_string()).map(|id| id.to_string()) else {
            return Err(err);
        };
        if to == self.chat_id {
            return Err(err);
        }
        *self.moved_to.lock() = Some(to.clone());
        self.send(&to, &text).await
    }

    fn taken_chat_migration(&self) -> Option<ChatMigration> {
        self.moved_to.lock().take().map(|to| ChatMigration {
            from: self.chat_id.clone(),
            to,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    const UPGRADED: &[u8] = br#"{"ok":false,"error_code":400,"description":"Bad Request: group chat was upgraded to a supergroup chat","parameters":{"migrate_to_chat_id":-1005}}"#;

    /// Head and body can arrive in separate segments, so read to
    /// `Content-Length` rather than trusting one `read`.
    async fn read_request_body(sock: &mut tokio::net::TcpStream) -> String {
        use tokio::io::AsyncReadExt;
        let mut raw = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            let n = sock.read(&mut chunk).await.unwrap_or(0);
            if n == 0 {
                break;
            }
            raw.extend_from_slice(&chunk[..n]);
            let text = String::from_utf8_lossy(&raw);
            let Some(split) = text.find("\r\n\r\n") else {
                continue;
            };
            let declared = text[..split]
                .lines()
                .find_map(|l| l.strip_prefix("Content-Length: "))
                .and_then(|v| v.trim().parse::<usize>().ok())
                .unwrap_or(0);
            if raw.len() >= split + 4 + declared {
                break;
            }
        }
        let text = String::from_utf8_lossy(&raw);
        text.split("\r\n\r\n").nth(1).unwrap_or("").to_string()
    }

    async fn upgraded_group_server() -> (Url, Arc<Mutex<Vec<String>>>) {
        use tokio::io::AsyncWriteExt;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let bodies = Arc::new(Mutex::new(Vec::new()));
        let seen = bodies.clone();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let body = read_request_body(&mut sock).await;
                let first = {
                    let mut seen = seen.lock().unwrap();
                    seen.push(body);
                    seen.len() == 1
                };
                let (status, payload): (&str, &[u8]) = if first {
                    ("400 Bad Request", UPGRADED)
                } else {
                    ("200 OK", br#"{"ok":true,"result":{}}"#)
                };
                let head = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    payload.len()
                );
                let _ = sock.write_all(head.as_bytes()).await;
                let _ = sock.write_all(payload).await;
                let _ = sock.shutdown().await;
            }
        });
        (
            Url::parse(&format!("http://{addr}/sendMessage")).unwrap(),
            bodies,
        )
    }

    #[tokio::test]
    async fn a_dead_group_id_is_followed_to_the_supergroup_in_the_same_send() {
        let (url, bodies) = upgraded_group_server().await;
        let http = crate::http_outbound::build_outbound_client(
            crate::security::SsrfGuard::relaxed_for_tests(),
        );
        let notifier = TelegramNotifier::at(http, url, "-5".into());
        let notice =
            crate::notifier::card::tests::notice(crate::domain::NotificationReason::Opened);

        notifier
            .notify_incident(&notice)
            .await
            .expect("the page lands in the supergroup");

        let bodies = bodies.lock().unwrap().clone();
        assert_eq!(bodies.len(), 2, "one send to the dead id, one to the new");
        assert!(bodies[0].contains(r#""chat_id":"-5""#), "{}", bodies[0]);
        assert!(bodies[1].contains(r#""chat_id":"-1005""#), "{}", bodies[1]);
        assert_eq!(
            notifier.taken_chat_migration(),
            Some(ChatMigration {
                from: "-5".into(),
                to: "-1005".into()
            })
        );
        assert_eq!(notifier.taken_chat_migration(), None, "taken once");
    }

    #[test]
    fn migration_hint_reads_the_new_chat_id() {
        let err = "endpoint returned 400 Bad Request: {\"ok\":false,\"error_code\":400,\
                   \"description\":\"Bad Request: group chat was upgraded to a supergroup chat\",\
                   \"parameters\":{\"migrate_to_chat_id\":-1004401963077}}";
        assert_eq!(migrated_chat_id(err), Some(-1004401963077));
        assert_eq!(
            migrated_chat_id("endpoint returned 400 Bad Request: {\"ok\":false}"),
            None
        );
        assert_eq!(migrated_chat_id("{\"migrate_to_chat_id\":\"abc\"}"), None);
    }
}
