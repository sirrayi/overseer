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

use std::collections::{BTreeMap, VecDeque};
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
    /// Subagent handles by task id: an interrupt here fans out to them.
    children: Mutex<BTreeMap<String, Control>>,
}

impl Control {
    /// Request the run to stop at the next safe boundary.
    /// Fans out to every registered child (hierarchical cancel).
    pub fn interrupt(&self) {
        self.inner.interrupt.store(true, Ordering::SeqCst);
        for child in self.children() {
            child.interrupt();
        }
    }

    fn children(&self) -> Vec<Control> {
        self.inner
            .children
            .lock()
            .map(|c| c.values().cloned().collect())
            .unwrap_or_default()
    }

    /// A fresh handle for subagent `id`, registered so this handle's
    /// interrupt reaches it. Born interrupted when this one already is.
    pub fn child(&self, id: &str) -> Control {
        let c = Control::default();
        if let Ok(mut m) = self.inner.children.lock() {
            m.insert(id.to_string(), c.clone());
        }
        if self.interrupted() {
            c.interrupt();
        }
        c
    }

    /// The registered handle of subagent `id`.
    pub fn child_of(&self, id: &str) -> Option<Control> {
        self.inner.children.lock().ok()?.get(id).cloned()
    }

    /// Drop subagent `id`'s registration (it finished).
    pub fn forget(&self, id: &str) {
        if let Ok(mut m) = self.inner.children.lock() {
            m.remove(id);
        }
    }

    /// Take over `other`'s children — a frontend that swaps in a new
    /// handle per run still reaches background tasks of earlier runs.
    pub fn adopt(&self, other: &Control) {
        if Arc::ptr_eq(&self.inner, &other.inner) {
            return;
        }
        let kids: Vec<(String, Control)> = other
            .inner
            .children
            .lock()
            .map(|c| c.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
            .unwrap_or_default();
        if let Ok(mut m) = self.inner.children.lock() {
            m.extend(kids);
        }
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
    fn interrupt_fans_out_to_children() {
        let parent = Control::default();
        let a = parent.child("task-1");
        let grand = a.child("task-1");
        let b = parent.child("task-2");
        parent.forget("task-2");
        parent.interrupt();
        assert!(a.interrupted() && grand.interrupted());
        assert!(!b.interrupted(), "a forgotten child is not reached");
        let next = Control::default();
        let c = parent.child("task-3");
        next.adopt(&parent);
        assert!(next.child_of("task-3").is_some());
        assert!(
            c.interrupted(),
            "born interrupted under an interrupted parent"
        );
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
