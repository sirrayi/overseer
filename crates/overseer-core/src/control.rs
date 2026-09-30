//! User-steering handle (P2.4, playbook Ch.9: "steering is delivered at
//! safe boundaries, not mid-tool").
//!
//! The frontend (TUI, proto client) holds a clone; the agent loop checks
//! it at two points only:
//!   - the top of each ReAct iteration (before the provider call), and
//!   - before each tool launch inside a batch.
//!
//! Skipped tool calls get synthetic results so the tool_use/tool_result
//! pairing survives an interruption (OpenClaw rule: every requested call
//! paired with a result). A single in-flight tool is never killed —
//! interrupt lands at the next launch boundary.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// Shared control channel between a frontend and a running agent.
/// Cheap to clone; `Default` is the never-interrupt headless handle.
#[derive(Debug, Default, Clone)]
pub struct Control {
    inner: Arc<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    interrupt: Arc<AtomicBool>,
    steer: Mutex<VecDeque<String>>,
}

impl Control {
    /// Request the run to stop at the next safe boundary.
    pub fn interrupt(&self) {
        self.inner.interrupt.store(true, Ordering::SeqCst);
    }

    pub fn interrupted(&self) -> bool {
        self.inner.interrupt.load(Ordering::SeqCst)
    }

    /// The bare interrupt flag, for watchers that must not hold the
    /// whole handle (the `run_code` engine's interrupt handler).
    pub fn interrupt_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.inner.interrupt)
    }

    /// Queue user input for delivery at the next boundary. Mid-run input
    /// redirects the agent ASAP (skipped calls get synthetic results);
    /// input queued while idle is picked up by the next `run_turn`.
    pub fn steer(&self, text: impl Into<String>) {
        if let Ok(mut q) = self.inner.steer.lock() {
            q.push_back(text.into());
        }
    }

    /// A steer is waiting — the loop skips remaining tool launches so the
    /// input lands at the next iteration, not after a full batch.
    pub fn steer_pending(&self) -> bool {
        self.inner
            .steer
            .lock()
            .map(|q| !q.is_empty())
            .unwrap_or(false)
    }

    /// Drain queued steering (loop-boundary delivery).
    pub fn take_steer(&self) -> Vec<String> {
        self.inner
            .steer
            .lock()
            .map(|mut q| q.drain(..).collect())
            .unwrap_or_default()
    }

    /// Peek at queued steering without draining (the UI's queue strip
    /// renders "queued ≠ sent").
    pub fn queued(&self) -> Vec<String> {
        self.inner
            .steer
            .lock()
            .map(|q| q.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Remove one queued message by index (queue-strip cancel). Returns
    /// the removed text when the index was valid.
    pub fn cancel_queued(&self, idx: usize) -> Option<String> {
        self.inner.steer.lock().ok().and_then(|mut q| q.remove(idx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interrupt_flag_is_shared() {
        let c = Control::default();
        let clone = c.clone();
        assert!(!c.interrupted());
        clone.interrupt();
        assert!(c.interrupted());
    }

    #[test]
    fn steer_queue_fifo_and_cancel() {
        let c = Control::default();
        c.steer("first");
        c.steer("second");
        c.steer("third");
        assert!(c.steer_pending());
        assert_eq!(c.queued(), vec!["first", "second", "third"]);
        assert_eq!(c.cancel_queued(1).as_deref(), Some("second"));
        assert_eq!(c.take_steer(), vec!["first", "third"]);
        assert!(!c.steer_pending());
        assert_eq!(c.cancel_queued(0), None);
    }
}
