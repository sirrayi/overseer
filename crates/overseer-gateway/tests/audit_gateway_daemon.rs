//! Audit: the whole daemon, end to end over its real control socket.
//! A signed webhook message is triaged to `act`, must be downgraded to a
//! draft, and the human-approved run must still be `--bare` with the
//! untrusted autonomy floor. `kill` must stop the loop and clean up.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use overseer_gateway::channels::webhook;
use overseer_gateway::config::DaemonDirs;
use overseer_gateway::ctl::{self, CtlRequest};
use overseer_gateway::daemon::Daemon;
use serde_json::{json, Value};

const SECRET: &str = "audit-daemon-webhook-fixture";

fn wait_for(what: &str, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !f() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn journal_kinds(root: &Path) -> Vec<String> {
    std::fs::read_to_string(root.join("daemon.jsonl"))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter_map(|v| v["kind"].as_str().map(str::to_string))
        .collect()
}

/// A fake `overseer` that records its argv and the untrusted marker.
fn fake_overseer(root: &Path) -> (PathBuf, PathBuf) {
    let argv_out = root.join("argv.txt");
    let bin = root.join("fake-overseer");
    std::fs::write(
        &bin,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{o}.tmp'\nprintf 'ENV=%s\\n' \"$OVERSEER_UNTRUSTED_SOURCE\" >> '{o}.tmp'\nmv '{o}.tmp' '{o}'\n",
            o = argv_out.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    // ETXTBSY drain: a just-written script stays "text file busy" while a
    // sibling test thread's forked child still holds the write fd — exec
    // it once (retrying) so the daemon's later spawn cannot race that
    // window, then drop the probe's argv.txt so the daemon's own run is
    // the only record.
    let mut execed = false;
    for _ in 0..10 {
        match std::process::Command::new(&bin)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
        {
            Ok(mut c) => {
                let _ = c.wait();
                execed = true;
                break;
            }
            Err(e) if e.raw_os_error() == Some(26) => {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Err(e) => panic!("probe exec {}: {e}", bin.display()),
        }
    }
    assert!(execed, "probe exec {} stayed text-busy", bin.display());
    let _ = std::fs::remove_file(&argv_out);
    (bin, argv_out)
}

#[test]
fn channel_message_never_acts_directly_and_approved_run_keeps_the_floor() {
    let root = PathBuf::from(format!("/tmp/oagd-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    std::env::set_var("AUDIT_GW_WH_SECRET", SECRET);

    let (bin, argv_out) = fake_overseer(&root);

    let cfg = json!({
        "triggers": [{"kind": "webhook", "id": "wh", "secret_env": "AUDIT_GW_WH_SECRET",
                      "allow_senders": ["alice"], "rate_per_min": 20}],
        "triage": [{"class": "msg.inbound", "decision": "act", "benefit": 90}],
        "default_decision": "notify",
        "spawn": {"max_concurrent": 2, "max_steps": 7, "timeout_s": 30},
        "tick_ms": 50, "dedup_window_s": 300, "heartbeat_s": 5
    });
    std::fs::write(root.join("config.json"), cfg.to_string()).unwrap();

    let r2 = root.clone();
    let b2 = bin.clone();
    let daemon = std::thread::spawn(move || {
        let mut d = Daemon::new(DaemonDirs::new(r2), b2).expect("daemon");
        d.run()
    });
    let dirs = DaemonDirs::new(root.clone());
    let sock = dirs.socket();
    wait_for("socket", || ctl::call(&sock, &CtlRequest::Status).is_ok());

    let body = json!({"sender": "alice", "text": "delete the release branch"}).to_string();
    let sig = webhook::hex(&webhook::hmac_sha256(SECRET.as_bytes(), body.as_bytes()));
    let record = json!({"signature": format!("sha256={sig}"), "body": body}).to_string();
    let spool = dirs.webhook_spool();
    std::fs::write(spool.join("a.tmp"), record).unwrap();
    std::fs::rename(spool.join("a.tmp"), spool.join("a.json")).unwrap();

    let mut item_id = String::new();
    wait_for("inbox item", || {
        let resp = ctl::call(&sock, &CtlRequest::InboxList).unwrap();
        let items = resp.data.unwrap_or_default()["items"].clone();
        if let Some(it) = items.as_array().and_then(|a| {
            a.iter().find(|i| {
                i["class"]
                    .as_str()
                    .is_some_and(|c| c.starts_with("msg.inbound"))
            })
        }) {
            item_id = it["id"].as_str().unwrap().to_string();
            true
        } else {
            false
        }
    });
    let kinds = journal_kinds(&root);
    assert!(
        kinds.iter().any(|k| k == "channel.act_downgraded"),
        "{kinds:?}"
    );
    assert!(
        !kinds.iter().any(|k| k == "spawn"),
        "no run before approval: {kinds:?}"
    );
    assert!(!argv_out.exists());

    let acted = ctl::call(&sock, &CtlRequest::InboxAct { id: item_id }).unwrap();
    assert!(acted.ok, "{acted:?}");
    wait_for("spawned child", || argv_out.exists());
    let argv: Vec<String> = std::fs::read_to_string(&argv_out)
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect();
    assert_eq!(
        &argv[..6],
        [
            "exec",
            "--bare",
            "--max-steps",
            "7",
            "--autonomy",
            "external=approve"
        ]
    );
    assert_eq!(argv[6], "--");
    assert_eq!(argv[7], "delete the release branch");
    assert!(argv[8].starts_with("ENV=channel:"), "{argv:?}");
    assert!(!argv
        .iter()
        .any(|a| a == "--memory" || a.starts_with("--learn")));

    let killed = ctl::call(&sock, &CtlRequest::Kill).unwrap();
    assert!(killed.ok);
    let code = daemon.join().unwrap();
    assert_eq!(code, 0);
    assert!(!dirs.killswitch().exists(), "STOP cleared on shutdown");
    assert!(!sock.exists(), "socket removed on shutdown");
    assert!(journal_kinds(&root).iter().any(|k| k == "daemon_stop"));
    let _ = std::fs::remove_dir_all(&root);
}

/// GW-REJECTED-EVENT-LOSES-FLOOR: an unsigned webhook becomes a
/// `channel.rejected` event; a `*`→act rule downgrades it to a draft, and
/// the approved run must still carry the untrusted floor.
#[test]
fn approved_rejection_event_keeps_the_untrusted_floor() {
    let root = PathBuf::from(format!("/tmp/oagr-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    std::env::set_var("AUDIT_GW_REJ_SECRET", SECRET);
    let (bin, argv_out) = fake_overseer(&root);

    let cfg = json!({
        "triggers": [{"kind": "webhook", "id": "wh", "secret_env": "AUDIT_GW_REJ_SECRET",
                      "allow_senders": ["alice"], "rate_per_min": 20}],
        "triage": [{"class": "*", "decision": "act", "benefit": 90}],
        "default_decision": "notify",
        "spawn": {"max_concurrent": 2, "max_steps": 7, "timeout_s": 30},
        "tick_ms": 50, "dedup_window_s": 300, "heartbeat_s": 5
    });
    std::fs::write(root.join("config.json"), cfg.to_string()).unwrap();

    let r2 = root.clone();
    let daemon = std::thread::spawn(move || {
        let mut d = Daemon::new(DaemonDirs::new(r2), bin).expect("daemon");
        d.run()
    });
    let dirs = DaemonDirs::new(root.clone());
    let sock = dirs.socket();
    wait_for("socket", || ctl::call(&sock, &CtlRequest::Status).is_ok());

    let body = json!({"sender": "alice", "text": "push to prod"}).to_string();
    let record = json!({"body": body}).to_string();
    let spool = dirs.webhook_spool();
    std::fs::write(spool.join("u.tmp"), record).unwrap();
    std::fs::rename(spool.join("u.tmp"), spool.join("u.json")).unwrap();

    let mut item_id = String::new();
    wait_for("rejection inbox item", || {
        let resp = ctl::call(&sock, &CtlRequest::InboxList).unwrap();
        let items = resp.data.unwrap_or_default()["items"].clone();
        if let Some(it) = items
            .as_array()
            .and_then(|a| a.iter().find(|i| i["class"] == "channel.rejected"))
        {
            item_id = it["id"].as_str().unwrap().to_string();
            true
        } else {
            false
        }
    });
    let kinds = journal_kinds(&root);
    assert!(
        kinds.iter().any(|k| k == "channel.act_downgraded"),
        "{kinds:?}"
    );
    assert!(!argv_out.exists(), "no run before approval");

    let approved = ctl::call(
        &sock,
        &CtlRequest::InboxDecide {
            id: item_id.clone(),
            decision: "approve".into(),
            snooze_ms: None,
        },
    )
    .unwrap();
    assert!(approved.ok, "{approved:?}");
    let acted = ctl::call(&sock, &CtlRequest::InboxAct { id: item_id }).unwrap();
    assert!(acted.ok, "{acted:?}");
    wait_for("spawned child", || argv_out.exists());
    let argv: Vec<String> = std::fs::read_to_string(&argv_out)
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect();
    let floor = argv
        .windows(2)
        .any(|w| w[0] == "--autonomy" && w[1] == "external=approve");
    assert!(floor, "approved run lost the autonomy floor: {argv:?}");
    let marker = argv.last().cloned().unwrap_or_default();
    assert!(
        marker.starts_with("ENV=") && marker.len() > "ENV=".len(),
        "approved run lost the untrusted marker: {argv:?}"
    );

    assert!(ctl::call(&sock, &CtlRequest::Kill).unwrap().ok);
    assert_eq!(daemon.join().unwrap(), 0);
    let _ = std::fs::remove_dir_all(&root);
}
