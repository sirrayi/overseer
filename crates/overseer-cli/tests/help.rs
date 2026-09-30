//! `--help`/`-h` must work on EVERY subcommand — the shared flag parser
//! rejects it as an unknown flag, so dispatch intercepts it.

use std::process::Command;

fn run(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_overseer"))
        .args(args)
        .output()
        .expect("run overseer")
}

fn assert_help(out: std::process::Output) {
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(text.contains("USAGE"), "no usage printed: {text}");
}

#[test]
fn web_help_prints_usage_and_exits_0() {
    assert_help(run(&["web", "--help"]));
}

#[test]
fn exec_short_h_prints_usage_and_exits_0() {
    assert_help(run(&["exec", "-h"]));
}

#[test]
fn daemon_help_prints_usage_and_exits_0() {
    assert_help(run(&["daemon", "--help"]));
}

#[test]
fn tui_help_prints_usage_and_exits_0() {
    assert_help(run(&["tui", "-h"]));
}

#[test]
fn memory_help_prints_usage_and_exits_0() {
    assert_help(run(&["memory", "--help"]));
}
