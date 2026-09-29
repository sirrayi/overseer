//! Helpers shared by the crate's unit tests.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

/// A fresh directory unique to this call (process id + counter + clock),
/// so parallel tests and reruns never share state.
pub fn tmpdir(tag: &str) -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "overseer-gw-{tag}-{}-{}-{}",
        std::process::id(),
        crate::event::now_ms(),
        N.fetch_add(1, Ordering::SeqCst)
    ));
    std::fs::create_dir_all(&dir).expect("create tmpdir");
    dir
}
