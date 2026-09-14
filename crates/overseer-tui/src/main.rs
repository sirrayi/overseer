//! overseer-tui — terminal frontend stub (Phase 2 deliverable).
//!
//! Phase 0 policy: TUI/headless parity — the TUI is a view over the same
//! event stream the `exec --json` mode emits, never a second engine.

fn main() {
    eprintln!(
        "overseer-tui is not built yet (Phase 2).\n\
         Use `overseer exec` — same engine, same event stream."
    );
    std::process::exit(2);
}
