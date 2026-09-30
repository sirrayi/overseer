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

/// Socket tests need roots that always fit `sockaddr_un.sun_path` (104
/// bytes on macOS/BSD, 108 on Linux): a runner's TMPDIR under
/// `~/actions-runner/_work/_temp/...` pushes the daemon's ctl socket
/// path past the limit. `/tmp` is the shortest absolute root — fall
/// back to `temp_dir()` when it isn't writable.
pub fn short_tmpdir(tag: &str) -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    let tag = &tag[..tag.len().min(8)];
    let uniq = (std::process::id() as u64)
        .wrapping_mul(0x9E37_79B9)
        .wrapping_add(crate::event::now_ms())
        .wrapping_add(N.fetch_add(1, Ordering::SeqCst) << 20);
    let dir = PathBuf::from("/tmp").join(format!("ovs-{tag}-{uniq:08x}", uniq = uniq as u32));
    match std::fs::create_dir(&dir) {
        Ok(()) => dir,
        Err(_) => tmpdir(tag),
    }
}
