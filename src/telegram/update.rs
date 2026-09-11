//! Incoming webhook update shapes and the pure classifier that turns one into
//! an intent. Only the fields the receiver acts on are modelled; everything
//! else is dropped so an unexpected payload still parses.

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct Update {
    #[serde(default)]
    pub message: Option<Message>,
    #[serde(default)]
    pub my_chat_member: Option<ChatMemberUpdated>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Message {
    #[serde(default)]
    pub text: Option<String>,
    pub chat: Chat,
    #[serde(default)]
    pub migrate_to_chat_id: Option<i64>,
    #[serde(default)]
    pub migrate_from_chat_id: Option<i64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Chat {
    pub id: i64,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub first_name: Option<String>,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default, rename = "type")]
    pub chat_type: Option<String>,
}

impl Chat {
    fn is_group(&self) -> bool {
        matches!(self.chat_type.as_deref(), Some("group" | "supergroup"))
    }

    /// Group title, else the private chat's person — private chats carry no
    /// `title`, and a bare chat id makes an anonymous channel.
    fn display_name(&self) -> Option<String> {
        self.title
            .clone()
            .or_else(|| self.first_name.clone())
            .or_else(|| self.username.clone())
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ChatMemberUpdated {
    pub chat: Chat,
    pub new_chat_member: ChatMember,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ChatMember {
    pub status: String,
}

/// The destination a link code resolves to, carried verbatim from the update.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatRef {
    pub id: i64,
    pub title: Option<String>,
}

impl ChatRef {
    fn from(chat: &Chat) -> Self {
        Self {
            id: chat.id,
            title: chat.display_name(),
        }
    }
}

/// What the receiver should do with an update. Identity (org, channel) is
/// never taken from here — only the link code and the chat it resolves to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WebhookAction {
    /// `/start <code>` in a private chat.
    LinkPrivate {
        code: String,
        chat: ChatRef,
    },
    /// `/start <code>` or `/link <code>` in a group the bot was added to.
    LinkGroup {
        code: String,
        chat: ChatRef,
    },
    /// `/stop` — the chat asked for alerts to end. Unlike [`Self::Removed`]
    /// the bot can still reply with a confirmation.
    Stop {
        chat_id: i64,
    },
    /// The bot was removed (`left`/`kicked`) from a chat.
    Removed {
        chat_id: i64,
    },
    Migrated {
        from: i64,
        to: i64,
    },
    /// Anything we don't act on — acknowledged and dropped.
    Ignore,
}

/// Split `/cmd@bot arg` into `(/cmd, arg)`, dropping the `@bot` suffix groups
/// add to commands. Returns `None` when the text isn't a command.
fn parse_command(text: &str) -> Option<(&str, &str)> {
    let text = text.trim();
    let mut parts = text.splitn(2, char::is_whitespace);
    let cmd = parts.next()?;
    if !cmd.starts_with('/') {
        return None;
    }
    let cmd = cmd.split('@').next().unwrap_or(cmd);
    let arg = parts.next().unwrap_or("").trim();
    Some((cmd, arg))
}

pub fn classify_update(update: &Update) -> WebhookAction {
    // Announced once in each chat; following it is idempotent.
    if let Some(msg) = &update.message {
        if let Some(to) = msg.migrate_to_chat_id {
            return WebhookAction::Migrated {
                from: msg.chat.id,
                to,
            };
        }
        if let Some(from) = msg.migrate_from_chat_id {
            return WebhookAction::Migrated {
                from,
                to: msg.chat.id,
            };
        }
    }
    if let Some(msg) = &update.message
        && let Some(text) = &msg.text
        && let Some((cmd, code)) = parse_command(text)
    {
        if cmd == "/stop" {
            return WebhookAction::Stop {
                chat_id: msg.chat.id,
            };
        }
        if !code.is_empty() && (cmd == "/start" || cmd == "/link") {
            let code = code.to_string();
            let chat = ChatRef::from(&msg.chat);
            // The chat the message arrived in decides the variant, not which
            // command was typed — `/link` in a private chat is still a
            // private link.
            return if msg.chat.is_group() {
                WebhookAction::LinkGroup { code, chat }
            } else {
                WebhookAction::LinkPrivate { code, chat }
            };
        }
    }
    if let Some(mcm) = &update.my_chat_member
        && matches!(mcm.new_chat_member.status.as_str(), "left" | "kicked")
    {
        return WebhookAction::Removed {
            chat_id: mcm.chat.id,
        };
    }
    WebhookAction::Ignore
}

#[cfg(test)]
mod tests {
    use super::*;

    fn classify(json: &str) -> WebhookAction {
        classify_update(&serde_json::from_str(json).expect("parse update"))
    }

    #[test]
    fn start_in_private_chat_links_private() {
        let action = classify(
            r#"{"message":{"text":"/start abc123","chat":{"id":42,"type":"private","first_name":"A"}}}"#,
        );
        assert_eq!(
            action,
            WebhookAction::LinkPrivate {
                code: "abc123".into(),
                chat: ChatRef {
                    id: 42,
                    // Private chats have no title — the person's name stands
                    // in so the linked channel isn't anonymous.
                    title: Some("A".into())
                },
            }
        );
    }

    #[test]
    fn private_chat_falls_back_to_username_then_nothing() {
        let action = classify(
            r#"{"message":{"text":"/start c","chat":{"id":1,"type":"private","username":"slim"}}}"#,
        );
        assert_eq!(
            action,
            WebhookAction::LinkPrivate {
                code: "c".into(),
                chat: ChatRef {
                    id: 1,
                    title: Some("slim".into())
                },
            }
        );
        let bare = classify(r#"{"message":{"text":"/start c","chat":{"id":2,"type":"private"}}}"#);
        assert_eq!(
            bare,
            WebhookAction::LinkPrivate {
                code: "c".into(),
                chat: ChatRef { id: 2, title: None },
            }
        );
    }

    #[test]
    fn start_in_group_links_group_with_title() {
        let action = classify(
            r#"{"message":{"text":"/start tok","chat":{"id":-100123,"type":"supergroup","title":"Ops"}}}"#,
        );
        assert_eq!(
            action,
            WebhookAction::LinkGroup {
                code: "tok".into(),
                chat: ChatRef {
                    id: -100123,
                    title: Some("Ops".into())
                },
            }
        );
    }

    #[test]
    fn link_command_in_group_strips_bot_suffix() {
        let action = classify(
            r#"{"message":{"text":"/link@uptimepagebot tok","chat":{"id":-9,"type":"group","title":"T"}}}"#,
        );
        assert_eq!(
            action,
            WebhookAction::LinkGroup {
                code: "tok".into(),
                chat: ChatRef {
                    id: -9,
                    title: Some("T".into())
                },
            }
        );
    }

    #[test]
    fn link_command_in_private_chat_links_private() {
        let action =
            classify(r#"{"message":{"text":"/link tok","chat":{"id":5,"type":"private"}}}"#);
        assert_eq!(
            action,
            WebhookAction::LinkPrivate {
                code: "tok".into(),
                chat: ChatRef { id: 5, title: None },
            }
        );
    }

    #[test]
    fn bare_start_without_code_is_ignored() {
        assert_eq!(
            classify(r#"{"message":{"text":"/start","chat":{"id":1,"type":"private"}}}"#),
            WebhookAction::Ignore
        );
    }

    #[test]
    fn plain_text_is_ignored() {
        assert_eq!(
            classify(r#"{"message":{"text":"hello","chat":{"id":1,"type":"private"}}}"#),
            WebhookAction::Ignore
        );
    }

    #[test]
    fn stop_command_in_any_chat_stops() {
        assert_eq!(
            classify(r#"{"message":{"text":"/stop","chat":{"id":7,"type":"private"}}}"#),
            WebhookAction::Stop { chat_id: 7 }
        );
        assert_eq!(
            classify(
                r#"{"message":{"text":"/stop@uptimepagebot","chat":{"id":-9,"type":"supergroup","title":"Ops"}}}"#
            ),
            WebhookAction::Stop { chat_id: -9 }
        );
    }

    #[test]
    fn kicked_member_is_removed() {
        assert_eq!(
            classify(
                r#"{"my_chat_member":{"chat":{"id":-7,"type":"supergroup"},"new_chat_member":{"status":"kicked"}}}"#
            ),
            WebhookAction::Removed { chat_id: -7 }
        );
    }

    #[test]
    fn supergroup_upgrade_names_both_ids_from_either_chat() {
        let from_old = classify(
            r#"{"message":{"chat":{"id":-4401963077,"type":"group","title":"Ops"},"migrate_to_chat_id":-1004401963077}}"#,
        );
        let from_new = classify(
            r#"{"message":{"chat":{"id":-1004401963077,"type":"supergroup","title":"Ops"},"migrate_from_chat_id":-4401963077}}"#,
        );
        let expected = WebhookAction::Migrated {
            from: -4401963077,
            to: -1004401963077,
        };
        assert_eq!(from_old, expected);
        assert_eq!(from_new, expected);
    }

    #[test]
    fn member_join_alone_is_ignored() {
        // The bot being added carries no code; the `/start` message does.
        assert_eq!(
            classify(
                r#"{"my_chat_member":{"chat":{"id":-7,"type":"supergroup"},"new_chat_member":{"status":"member"}}}"#
            ),
            WebhookAction::Ignore
        );
    }

    #[test]
    fn unrelated_update_parses_and_ignores() {
        assert_eq!(
            classify(r#"{"edited_message":{"foo":1}}"#),
            WebhookAction::Ignore
        );
    }
}
