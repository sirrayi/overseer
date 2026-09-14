//! Canonical turn IR (playbook Ch.2 §2.5, Ch.7 §2.1).
//!
//! Provider-agnostic message model. The load-bearing decision: reasoning blocks
//! are carried as **opaque provider blobs** — Anthropic signed thinking blocks,
//! OpenAI encrypted reasoning items, Gemini thought signatures all round-trip
//! verbatim without the engine ever inspecting them.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
}

/// A typed content block. `Reasoning` stores the raw provider block so it can be
/// echoed back byte-identically (Anthropic 400s on rebuilt thinking blocks).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Block {
    Text {
        text: String,
    },
    /// Opaque reasoning/thinking block. `raw` is the provider's original block
    /// (e.g. Anthropic `{"type":"thinking","thinking":...,"signature":...}`).
    Reasoning {
        raw: serde_json::Value,
    },
    ToolCall {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
        #[serde(default)]
        is_error: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Vec<Block>,
}

impl Message {
    pub fn user_text(text: impl Into<String>) -> Self {
        Message {
            role: Role::User,
            content: vec![Block::Text { text: text.into() }],
        }
    }

    /// A user message carrying tool results (Anthropic requires results in the
    /// user role, tool_result blocks before any text).
    pub fn tool_results(results: Vec<Block>) -> Self {
        Message {
            role: Role::User,
            content: results,
        }
    }

    pub fn tool_calls(&self) -> impl Iterator<Item = (&str, &str, &serde_json::Value)> {
        self.content.iter().filter_map(|b| match b {
            Block::ToolCall { id, name, input } => Some((id.as_str(), name.as_str(), input)),
            _ => None,
        })
    }

    pub fn has_tool_calls(&self) -> bool {
        self.content
            .iter()
            .any(|b| matches!(b, Block::ToolCall { .. }))
    }

    pub fn text(&self) -> String {
        self.content
            .iter()
            .filter_map(|b| match b {
                Block::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("")
    }
}

/// Normalized usage accounting (playbook Ch.7 §2.1 item 6): every provider's
/// usage schema maps onto these five classes so cost tracking is uniform.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub fresh_input: u64,
    pub cache_write: u64,
    pub cache_read: u64,
    pub output: u64,
    pub reasoning: u64,
}

impl Usage {
    pub fn total_input(&self) -> u64 {
        self.fresh_input + self.cache_write + self.cache_read
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reasoning_block_roundtrips_raw() {
        let raw = serde_json::json!({
            "type": "thinking",
            "thinking": "let me think",
            "signature": "sig-abc"
        });
        let block = Block::Reasoning { raw: raw.clone() };
        let json = serde_json::to_value(&block).unwrap();
        let back: Block = serde_json::from_value(json).unwrap();
        match back {
            Block::Reasoning { raw: r } => assert_eq!(r, raw),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn tool_call_iterator() {
        let m = Message {
            role: Role::Assistant,
            content: vec![
                Block::Text { text: "hi".into() },
                Block::ToolCall {
                    id: "t1".into(),
                    name: "read".into(),
                    input: serde_json::json!({"path": "a"}),
                },
            ],
        };
        let calls: Vec<_> = m.tool_calls().collect();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].1, "read");
        assert!(m.has_tool_calls());
        assert_eq!(m.text(), "hi");
    }
}
