//! Telegram channel (P7-4): long-poll in, send out, no SDK.
//!
//! The whole channel is `ureq` against the bot HTTP API — the same
//! dependency `overseer-core` already uses, so the workspace gains no
//! package (R1-F1). The bot token is read from the environment by the
//! caller ([`super::sentinel::load_token`]) and never logged: every error
//! string this module produces is passed through the sentinel redactor
//! first, because Telegram's API embeds the token in the URL.

use std::time::Duration;

use serde_json::Value;

use super::sentinel;
use super::webhook::Inbound;
use crate::config::TelegramSpec;

/// One poll's worth of inbound work: the parsed messages plus the offset
/// the next poll must use (so a restart never re-delivers).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Polled {
    pub messages: Vec<Inbound>,
    pub next_offset: Option<i64>,
}

/// Whole-request bound for every Bot API call: one hung connection must
/// not wedge the single-threaded daemon loop.
pub const HTTP_TIMEOUT: Duration = Duration::from_secs(15);

/// `getUpdates` server hold time. Must stay below [`HTTP_TIMEOUT`], or a
/// healthy long-poll would be cut off as a timeout.
pub const LONG_POLL_S: u64 = 0;

/// The HTTP surface of one bot. `base` is overridable so tests can point
/// at a closed local port instead of the network.
#[derive(Debug, Clone)]
pub struct Telegram {
    token: String,
    base: String,
    timeout: Duration,
}

impl Telegram {
    /// Build a client from a token that is already in hand.
    pub fn new(token: impl Into<String>) -> Self {
        Telegram {
            token: token.into(),
            base: "https://api.telegram.org".to_string(),
            timeout: HTTP_TIMEOUT,
        }
    }

    /// Build from an environment variable. Unset/blank → an honest
    /// `unconfigured` error naming the variable (never a silent no-op).
    pub fn from_env(var: &str) -> Result<Self, String> {
        let token = sentinel::load_token(var)
            .ok_or_else(|| format!("telegram: {var} is unset — no bot token to poll with"))?;
        Ok(Self::new(token))
    }

    /// Point the client at a different API host (tests, self-hosted
    /// Bot-API-compatible servers).
    pub fn with_base(mut self, base: impl Into<String>) -> Self {
        self.base = base.into();
        self
    }

    /// Override the whole-request timeout (tests use a short bound).
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    fn agent(&self) -> ureq::Agent {
        ureq::Agent::config_builder()
            .timeout_global(Some(self.timeout))
            .build()
            .into()
    }

    fn url(&self, method: &str) -> String {
        format!(
            "{}/bot{}/{}",
            self.base.trim_end_matches('/'),
            self.token,
            method
        )
    }

    /// Long-poll URL for `getUpdates`. `timeout_s` is the *server* hold
    /// time; the daemon keeps it at 0 so a tick never blocks on the
    /// network.
    pub fn get_updates_url(&self, offset: Option<i64>, timeout_s: u64) -> String {
        let mut url = format!("{}?timeout={timeout_s}", self.url("getUpdates"));
        if let Some(o) = offset {
            url.push_str(&format!("&offset={o}"));
        }
        url
    }

    /// One non-blocking poll. A transport or API failure is an error naming
    /// the method, with the token redacted out of the message.
    pub fn poll(&self, offset: Option<i64>) -> Result<Polled, String> {
        let url = self.get_updates_url(offset, LONG_POLL_S);
        let body = self
            .agent()
            .get(&url)
            .call()
            .map_err(|e| self.scrub(format!("telegram: getUpdates failed: {e}")))?
            .body_mut()
            .read_to_string()
            .map_err(|e| self.scrub(format!("telegram: getUpdates body: {e}")))?;
        let v: Value = serde_json::from_str(&body)
            .map_err(|e| self.scrub(format!("telegram: getUpdates returned non-JSON: {e}")))?;
        if v.get("ok").and_then(Value::as_bool) == Some(false) {
            return Err(self.scrub(format!(
                "telegram: getUpdates refused: {}",
                v.get("description")
                    .and_then(Value::as_str)
                    .unwrap_or("no description")
            )));
        }
        Ok(Polled {
            messages: parse_updates(&v),
            next_offset: next_offset(&v, offset),
        })
    }

    /// Send a message. Outbound is always a *plain text* send — no parse
    /// mode, no reply markup — so nothing in the text can be interpreted
    /// by the channel.
    pub fn send(&self, chat: &str, text: &str) -> Result<(), String> {
        let payload = serde_json::json!({
            "chat_id": chat,
            "text": text,
            "disable_web_page_preview": true,
        });
        let resp = self
            .agent()
            .post(&self.url("sendMessage"))
            .send_json(&payload)
            .map_err(|e| self.scrub(format!("telegram: sendMessage failed: {e}")))?
            .body_mut()
            .read_to_string()
            .map_err(|e| self.scrub(format!("telegram: sendMessage body: {e}")))?;
        let v: Value = serde_json::from_str(&resp)
            .map_err(|e| self.scrub(format!("telegram: sendMessage returned non-JSON: {e}")))?;
        if v.get("ok").and_then(Value::as_bool) == Some(false) {
            return Err(self.scrub(format!(
                "telegram: sendMessage refused: {}",
                v.get("description")
                    .and_then(Value::as_str)
                    .unwrap_or("no description")
            )));
        }
        Ok(())
    }

    /// Strip the token from an error string before it can reach a log.
    pub fn scrub(&self, text: String) -> String {
        sentinel::redact(&text, std::slice::from_ref(&self.token))
    }

    /// Resolve the client for a spec: configured token or an honest error.
    pub fn for_spec(spec: &TelegramSpec) -> Result<Self, String> {
        Self::from_env(&spec.token_env)
    }
}

/// The offset the next `getUpdates` call must carry: highest `update_id`
/// seen plus one. `None` when the batch is empty or unparsable, so the
/// caller keeps its current offset (never rewinds to "deliver everything").
pub fn next_offset(v: &Value, current: Option<i64>) -> Option<i64> {
    let max = v
        .get("result")
        .and_then(Value::as_array)
        .and_then(|updates| {
            updates
                .iter()
                .filter_map(|u| u.get("update_id").and_then(Value::as_i64))
                .max()
        })?;
    let next = max + 1;
    Some(match current {
        Some(c) if c > next => c,
        _ => next,
    })
}

/// Extract inbound messages from a `getUpdates` body. Unsupported update
/// kinds (edits, joins, callbacks) are skipped; the chat id is the thread
/// key, so a group and a DM never share a session.
pub fn parse_updates(v: &Value) -> Vec<Inbound> {
    let Some(updates) = v.get("result").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for u in updates {
        let Some(msg) = u.get("message") else {
            continue;
        };
        let Some(text) = msg.get("text").and_then(Value::as_str) else {
            continue;
        };
        if text.trim().is_empty() {
            continue;
        }
        let Some(chat_id) = msg
            .get("chat")
            .and_then(|c| c.get("id"))
            .and_then(Value::as_i64)
        else {
            continue;
        };
        let sender = msg
            .get("from")
            .and_then(|f| f.get("id"))
            .and_then(Value::as_i64)
            .map(|id| id.to_string())
            .or_else(|| {
                msg.get("from")
                    .and_then(|f| f.get("username"))
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .unwrap_or_else(|| chat_id.to_string());
        out.push(Inbound {
            channel: "telegram".into(),
            sender,
            thread: Some(chat_id.to_string()),
            text: text.to_string(),
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn get_updates_url_carries_offset_and_timeout() {
        let tg = Telegram::new("T0K3N");
        assert_eq!(
            tg.get_updates_url(None, 0),
            "https://api.telegram.org/botT0K3N/getUpdates?timeout=0"
        );
        assert_eq!(
            tg.get_updates_url(Some(42), 25),
            "https://api.telegram.org/botT0K3N/getUpdates?timeout=25&offset=42"
        );
        // A trailing slash on the base must not double up.
        let tg = Telegram::new("T").with_base("http://127.0.0.1:9/");
        assert_eq!(
            tg.get_updates_url(None, 0),
            "http://127.0.0.1:9/botT/getUpdates?timeout=0"
        );
    }

    #[test]
    fn parse_updates_extracts_messages_and_skips_noise() {
        let body = serde_json::json!({
            "ok": true,
            "result": [
                {"update_id": 1, "message": {
                    "message_id": 10,
                    "chat": {"id": -1001, "type": "group"},
                    "from": {"id": 77},
                    "text": "hello room"
                }},
                {"update_id": 2, "edited_message": {"chat": {"id": 5}, "text": "edited"}},
                {"update_id": 3, "message": {"message_id": 11, "chat": {"id": 77}, "text": "   "}},
                {"update_id": 4, "message": {
                    "message_id": 12,
                    "chat": {"id": 77, "type": "private"},
                    "from": {"username": "noa"},
                    "text": "/queue hold"
                }},
                {"update_id": 5, "message": {"message_id": 13, "chat": {"id": 8}}}
            ]
        });
        let got = parse_updates(&body);
        assert_eq!(got.len(), 2);
        // The offset advances past the highest update id, and never rewinds.
        assert_eq!(next_offset(&body, None), Some(6));
        assert_eq!(next_offset(&body, Some(99)), Some(99));
        assert_eq!(
            got[0],
            Inbound {
                channel: "telegram".into(),
                sender: "77".into(),
                thread: Some("-1001".into()),
                text: "hello room".into(),
            }
        );
        // A message with no numeric `from.id` falls back to the username.
        assert_eq!(got[1].sender, "noa");
        assert_eq!(got[1].thread.as_deref(), Some("77"));
        // A body with no `result` yields nothing (never an error).
        assert!(parse_updates(&serde_json::json!({"ok": true})).is_empty());
    }

    #[test]
    fn poll_against_a_silent_server_errors_within_the_bound() {
        // A peer that accepts and never answers must not wedge the
        // single-threaded daemon loop.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let mut held = Vec::new();
            for conn in listener.incoming().flatten() {
                held.push(conn);
            }
        });
        let bound = std::time::Duration::from_millis(500);
        let tg = Telegram::new("SUPER-SECRET-TOKEN")
            .with_base(format!("http://{addr}"))
            .with_timeout(bound);
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let started = std::time::Instant::now();
            let res = tg.poll(None);
            let _ = tx.send((res, started.elapsed()));
        });
        let (res, took) = rx
            .recv_timeout(bound * 10)
            .expect("telegram poll hung on a silent server");
        let err = res.expect_err("a silent server is a transport error");
        assert!(err.contains("getUpdates"), "{err}");
        assert!(!err.contains("SUPER-SECRET-TOKEN"), "token leaked: {err}");
        assert!(took < bound * 4, "poll took {took:?}, bound {bound:?}");
    }

    #[test]
    fn default_timeout_outlasts_the_long_poll_hold() {
        let tg = Telegram::new("T");
        assert_eq!(tg.timeout, HTTP_TIMEOUT);
        assert_eq!(HTTP_TIMEOUT, std::time::Duration::from_secs(15));
        assert!(std::time::Duration::from_secs(LONG_POLL_S) < tg.timeout);
        assert!(tg
            .get_updates_url(None, LONG_POLL_S)
            .ends_with(&format!("timeout={LONG_POLL_S}")));
    }

    #[test]
    fn unconfigured_token_and_transport_failures_are_honest() {
        let err = Telegram::from_env("OVERSEER_TELEGRAM_BOT_TOKEN_UNSET_5d2").unwrap_err();
        assert!(err.contains("unset"), "got: {err}");
        assert!(err.contains("OVERSEER_TELEGRAM_BOT_TOKEN_UNSET_5d2"));
        // A closed loopback port: the transport error is a real error, and
        // the token never appears in it.
        let tg = Telegram::new("SUPER-SECRET-TOKEN").with_base("http://127.0.0.1:1");
        let err = tg.poll(None).unwrap_err();
        assert!(!err.contains("SUPER-SECRET-TOKEN"), "token leaked: {err}");
        if let Some(idx) = err.find("ovsent_") {
            let cand: String = err[idx..]
                .chars()
                .take(sentinel::SENTINEL_PREFIX.len() + sentinel::SENTINEL_HEX)
                .collect();
            assert!(
                sentinel::is_sentinel(&cand),
                "redacted token must be a well-formed sentinel: {err}"
            );
        }
        // Same for the outbound path.
        let err = tg.send("123", "hi").unwrap_err();
        assert!(err.contains("sendMessage"), "got: {err}");
        assert!(!err.contains("SUPER-SECRET-TOKEN"), "token leaked: {err}");
        // The URL builder is the only place the token is allowed to appear.
        assert!(tg.get_updates_url(None, 0).contains("SUPER-SECRET-TOKEN"));
    }
}
