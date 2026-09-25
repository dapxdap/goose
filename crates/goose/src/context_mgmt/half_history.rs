//! Splits a conversation into the half that gets summarized and the half that
//! is carried through compaction verbatim.
//!
//! Summarizing the whole history makes the retained context as large as the
//! summary the model feels like writing. Summarizing only the older half keeps
//! the recent half byte-for-byte, so the size of the result is bounded by
//! something already known rather than by the summarizer's output budget.

use crate::conversation::message::{Message, MessageContent};
use crate::token_counter::create_token_counter;
use tracing::info;

/// Token budget for the summarized (older) half. A summarizer with a small
/// context window cannot take a proportional slice of a very long history, so
/// the split is a fixed budget rather than a share of the total.
const OLDER_HALF_TOKEN_BUDGET: usize = 50_000;

/// Minimum messages left verbatim, so a compaction can never reduce the
/// conversation to a summary alone.
const MIN_RETAINED_MESSAGES: usize = 2;

fn tool_request_ids(msg: &Message) -> Vec<&str> {
    msg.content
        .iter()
        .filter_map(|c| match c {
            MessageContent::ToolRequest(req) => Some(req.id.as_str()),
            _ => None,
        })
        .collect()
}

fn tool_response_ids(msg: &Message) -> Vec<&str> {
    msg.content
        .iter()
        .filter_map(|c| match c {
            MessageContent::ToolResponse(res) => Some(res.id.as_str()),
            _ => None,
        })
        .collect()
}

/// Index of the first message kept verbatim, or `None` when the history is too
/// short for a split to be worth it.
///
/// The boundary is where the oldest `OLDER_HALF_TOKEN_BUDGET` tokens end, walked
/// back until it no longer separates a tool request from its response: a
/// response left in the retained half without its request is a message the
/// provider cannot render.
pub async fn retained_split(messages: &[Message]) -> Option<usize> {
    let visible: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter(|(_, msg)| msg.is_agent_visible())
        .map(|(idx, _)| idx)
        .collect();

    if visible.len() < MIN_RETAINED_MESSAGES * 2 {
        info!(
            visible = visible.len(),
            "Compaction split skipped: history too short"
        );
        return None;
    }

    let counter = match create_token_counter().await {
        Ok(counter) => counter,
        Err(e) => {
            info!("Compaction split skipped: token counter unavailable: {e}");
            return None;
        }
    };
    let weights: Vec<usize> = visible
        .iter()
        .map(|&idx| counter.count_chat_tokens("", std::slice::from_ref(&messages[idx]), &[]))
        .collect();
    let total: usize = weights.iter().sum();
    if total == 0 {
        info!("Compaction split skipped: zero token weight");
        return None;
    }

    let mut running = 0usize;
    let mut position = visible.len() - MIN_RETAINED_MESSAGES;
    for (candidate, weight) in weights.iter().enumerate() {
        if running + weight > OLDER_HALF_TOKEN_BUDGET {
            position = candidate;
            break;
        }
        running += weight;
    }

    while position > 0 {
        let answered: Vec<&str> = messages[visible[position]..]
            .iter()
            .flat_map(tool_response_ids)
            .collect();
        let orphaned = messages[..visible[position]]
            .iter()
            .flat_map(tool_request_ids)
            .any(|id| answered.contains(&id));
        if !orphaned {
            break;
        }
        position -= 1;
    }

    (position > 0 && position < visible.len()).then(|| visible[position])
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::model::{CallToolRequestParams, ContentBlock};

    fn big_text(len: usize) -> String {
        "word ".repeat(len)
    }

    fn user(text: &str) -> Message {
        Message::user().with_text(text)
    }

    fn tool_pair(id: &str, payload: &str) -> Vec<Message> {
        vec![
            Message::assistant()
                .with_tool_request(id.to_string(), Ok(CallToolRequestParams::new("shell"))),
            Message::user().with_tool_response(
                id.to_string(),
                Ok(rmcp::model::CallToolResult::success(vec![
                    ContentBlock::text(payload),
                ])),
            ),
        ]
    }

    #[tokio::test]
    async fn short_history_is_not_split() {
        assert_eq!(retained_split(&[user("a"), user("b")]).await, None);
    }

    #[tokio::test]
    async fn split_keeps_recent_half() {
        let messages: Vec<Message> = (0..12)
            .map(|i| user(&format!("{i} {}", big_text(200))))
            .collect();
        let split = retained_split(&messages)
            .await
            .expect("long enough to split");
        assert!(
            split > 0 && split < messages.len(),
            "split {split} out of range"
        );
    }

    #[tokio::test]
    async fn split_never_orphans_a_tool_response() {
        let mut messages = vec![user(&big_text(400))];
        for i in 0..8 {
            messages.extend(tool_pair(&format!("call-{i}"), &big_text(120)));
        }
        messages.push(user(&big_text(400)));

        let split = retained_split(&messages)
            .await
            .expect("long enough to split");
        let answered: Vec<&str> = messages[split..]
            .iter()
            .flat_map(tool_response_ids)
            .collect();
        let orphaned = messages[..split]
            .iter()
            .flat_map(tool_request_ids)
            .any(|id| answered.contains(&id));
        assert!(!orphaned, "tool pair split at {split}");
    }

    #[tokio::test]
    async fn split_boundary_is_agent_visible() {
        let mut messages: Vec<Message> = (0..6)
            .map(|i| user(&format!("{i} {}", big_text(200))))
            .collect();
        messages.push(user("hidden").with_visibility(false, false));
        messages.extend((0..6).map(|i| user(&format!("tail-{i} {}", big_text(200)))));

        let split = retained_split(&messages)
            .await
            .expect("long enough to split");
        assert!(
            messages[split].is_agent_visible(),
            "boundary landed on a non-agent-visible message"
        );
    }
}
