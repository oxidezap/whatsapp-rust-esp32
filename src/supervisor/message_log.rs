use std::collections::VecDeque;

use crate::{serde, serde_json};

/// One inbound message as the dashboard and `/messages` show it.
#[derive(serde::Serialize)]
#[serde(crate = "crate::serde")]
pub struct MessageLogEntry {
    pub id: String,
    pub chat: String,
    pub sender: String,
    pub text: Option<String>,
    pub timestamp: i64,
    pub from_me: bool,
}

pub(super) fn to_json(messages: &VecDeque<MessageLogEntry>) -> String {
    #[derive(serde::Serialize)]
    #[serde(crate = "crate::serde")]
    struct View<'a> {
        count: usize,
        messages: &'a VecDeque<MessageLogEntry>,
    }

    // Polling can overlap Signal work on the C3. Borrow the bounded log
    // directly instead of cloning it into two temporary Value trees.
    serde_json::to_string(&View {
        count: messages.len(),
        messages,
    })
    .expect("message log contains only JSON scalar fields")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_message_schema_order_nulls_and_escaping() {
        let messages = VecDeque::from([
            MessageLogEntry {
                id: "first".into(),
                chat: "100000000001@s.whatsapp.net".into(),
                sender: "100000000002@s.whatsapp.net".into(),
                text: Some("quoted \"text\"\n\0🦀".into()),
                timestamp: -1,
                from_me: false,
            },
            MessageLogEntry {
                id: "second".into(),
                chat: "100000000001@s.whatsapp.net".into(),
                sender: "100000000001@s.whatsapp.net".into(),
                text: None,
                timestamp: i64::MAX,
                from_me: true,
            },
        ]);
        let value: serde_json::Value = serde_json::from_str(&to_json(&messages)).unwrap();
        assert_eq!(value["count"], 2);
        assert_eq!(value["messages"][0]["id"], "first");
        assert_eq!(value["messages"][0]["chat"], messages[0].chat);
        assert_eq!(value["messages"][0]["sender"], messages[0].sender);
        assert_eq!(
            value["messages"][0]["text"],
            messages[0].text.as_deref().unwrap()
        );
        assert_eq!(value["messages"][0]["timestamp"], -1);
        assert_eq!(value["messages"][0]["from_me"], false);
        assert_eq!(value["messages"][1]["id"], "second");
        assert!(value["messages"][1]["text"].is_null());
        assert_eq!(value["messages"][1]["timestamp"], i64::MAX);
        assert_eq!(value["messages"][1]["from_me"], true);
        assert_eq!(value["messages"][0].as_object().unwrap().len(), 6);
    }

    #[test]
    fn empty_log_keeps_the_response_shape() {
        let value: serde_json::Value = serde_json::from_str(&to_json(&VecDeque::new())).unwrap();
        assert_eq!(value, serde_json::json!({"count": 0, "messages": []}));
    }
}
