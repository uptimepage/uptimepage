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
    #[serde(default)]
    pub callback_query: Option<CallbackQuery>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Message {
    #[serde(default)]
    pub message_id: i64,
    #[serde(default)]
    pub text: Option<String>,
    pub chat: Chat,
    #[serde(default)]
    pub from: Option<User>,
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

/// A Telegram account. Telegram fills it in itself, so unlike callback data a
/// client cannot claim to be someone else.
#[derive(Debug, Clone, Deserialize)]
pub struct User {
    pub id: i64,
    #[serde(default)]
    pub first_name: Option<String>,
    #[serde(default)]
    pub username: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CallbackQuery {
    pub id: String,
    pub from: User,
    /// Absent when the message is too old for Telegram to send along.
    #[serde(default)]
    pub message: Option<CallbackMessage>,
    #[serde(default)]
    pub data: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CallbackMessage {
    pub message_id: i64,
    pub chat: Chat,
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

/// A press on a button. The data is whatever the client sent; the person and
/// the chat are Telegram's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Press {
    pub query_id: String,
    pub person: Person,
    pub chat_id: i64,
    /// Other people read the chat, as opposed to a private one with the bot.
    pub group: bool,
    pub message_id: i64,
    pub data: String,
}

/// Whoever pressed a button or sent a command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Person {
    pub id: i64,
    /// First name, which is what a chat shows for them.
    pub name: Option<String>,
    pub username: Option<String>,
}

impl Person {
    fn from(user: &User) -> Self {
        Self {
            id: user.id,
            name: user.first_name.clone(),
            username: user.username.clone(),
        }
    }

    /// How to refer to them where the chat can read it.
    pub fn display(&self) -> Option<String> {
        self.name
            .clone()
            .or_else(|| self.username.as_ref().map(|u| format!("@{u}")))
    }

    /// How to label the account on their settings page, where the handle is
    /// what tells two accounts apart.
    pub fn label(&self) -> Option<String> {
        self.username
            .as_ref()
            .map(|u| format!("@{u}"))
            .or_else(|| self.name.clone())
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
    /// `/start me-<code>` in a private chat: a code that links the sender's
    /// Telegram account to a person rather than a chat to an org.
    LinkAccount {
        code: String,
        person: Person,
        chat_id: i64,
    },
    /// `/unlink` in a private chat: free the sender's Telegram account from
    /// whoever it is linked to. Telegram vouches for the sender, so only the
    /// account itself can do this.
    UnlinkAccount {
        person: Person,
        chat_id: i64,
    },
    /// A button on one of the bot's messages.
    Pressed(Press),
    /// A button press with nothing to act on. Still answered, or the button
    /// spins until Telegram gives up.
    Unanswerable {
        query_id: String,
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
    if let Some(q) = &update.callback_query {
        return match (&q.message, &q.data) {
            (Some(msg), Some(data)) => WebhookAction::Pressed(Press {
                query_id: q.id.clone(),
                person: Person::from(&q.from),
                chat_id: msg.chat.id,
                group: msg.chat.is_group(),
                message_id: msg.message_id,
                data: data.clone(),
            }),
            _ => WebhookAction::Unanswerable {
                query_id: q.id.clone(),
            },
        };
    }
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
        if cmd == "/unlink"
            && !msg.chat.is_group()
            && let Some(from) = &msg.from
        {
            return WebhookAction::UnlinkAccount {
                person: Person::from(from),
                chat_id: msg.chat.id,
            };
        }
        if cmd == "/start"
            && !msg.chat.is_group()
            && let Some(code) = crate::security::app_link::telegram_start_code(code)
            && let Some(from) = &msg.from
        {
            return WebhookAction::LinkAccount {
                code: code.to_string(),
                person: Person::from(from),
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
    fn a_button_press_carries_telegrams_person_and_chat() {
        let action = classify(
            r#"{"callback_query":{"id":"q1","from":{"id":77,"first_name":"Olena","username":"olena_k"},
                "message":{"message_id":9,"chat":{"id":-100,"type":"supergroup","title":"Ops"}},
                "data":"aXYZ"}}"#,
        );
        assert_eq!(
            action,
            WebhookAction::Pressed(Press {
                query_id: "q1".into(),
                person: Person {
                    id: 77,
                    name: Some("Olena".into()),
                    username: Some("olena_k".into()),
                },
                chat_id: -100,
                group: true,
                message_id: 9,
                data: "aXYZ".into(),
            })
        );
    }

    #[test]
    fn a_press_without_its_message_or_data_is_still_answered() {
        let unanswerable = WebhookAction::Unanswerable {
            query_id: "q".into(),
        };
        assert_eq!(
            classify(r#"{"callback_query":{"id":"q","from":{"id":1},"data":"a"}}"#),
            unanswerable
        );
        assert_eq!(
            classify(
                r#"{"callback_query":{"id":"q","from":{"id":1},"message":{"message_id":1,"chat":{"id":1}}}}"#
            ),
            unanswerable
        );
    }

    #[test]
    fn an_account_code_in_a_private_chat_links_the_sender() {
        let code = crate::security::token_hash::generate_raw_token();
        let payload = crate::security::app_link::telegram_start_payload(&code);
        let json = format!(
            r#"{{"message":{{"text":"/start {payload}","chat":{{"id":77,"type":"private"}},"from":{{"id":77,"first_name":"Olena"}}}}}}"#
        );
        assert_eq!(
            classify(&json),
            WebhookAction::LinkAccount {
                code: code.clone(),
                person: Person {
                    id: 77,
                    name: Some("Olena".into()),
                    username: None,
                },
                chat_id: 77,
            }
        );

        let in_group = format!(
            r#"{{"message":{{"text":"/start {payload}","chat":{{"id":-5,"type":"group","title":"Ops"}},"from":{{"id":77}}}}}}"#
        );
        assert!(
            matches!(classify(&in_group), WebhookAction::LinkGroup { .. }),
            "a group only ever links a chat"
        );
    }

    #[test]
    fn unlink_frees_only_the_sender_and_only_in_private() {
        assert_eq!(
            classify(
                r#"{"message":{"text":"/unlink","chat":{"id":77,"type":"private"},"from":{"id":77,"first_name":"Olena"}}}"#
            ),
            WebhookAction::UnlinkAccount {
                person: Person {
                    id: 77,
                    name: Some("Olena".into()),
                    username: None,
                },
                chat_id: 77,
            }
        );
        assert_eq!(
            classify(
                r#"{"message":{"text":"/unlink@uptimepagebot","chat":{"id":-5,"type":"group","title":"Ops"},"from":{"id":77}}}"#
            ),
            WebhookAction::Ignore
        );
    }

    #[test]
    fn a_person_reads_as_the_chat_names_them() {
        let both = Person {
            id: 1,
            name: Some("Olena".into()),
            username: Some("olena_k".into()),
        };
        assert_eq!(both.display().as_deref(), Some("Olena"));
        assert_eq!(both.label().as_deref(), Some("@olena_k"));
        let handle_only = Person {
            id: 1,
            name: None,
            username: Some("olena_k".into()),
        };
        assert_eq!(handle_only.display().as_deref(), Some("@olena_k"));
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
