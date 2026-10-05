//! Connector integration tests: mock loopback HTTP, fixture credentials
//! under a temp HOME, fake command runner. No real network, home dir, or
//! keychain is touched. Fixture secrets are distinctively marked so the
//! redaction test can prove they never leak into output.

mod common;

use std::collections::HashSet;

use common::{test_ctx, FakeRunner, MockServer, TestHome};
use overseer_life::connector::Connector;
use overseer_life::connectors::x::TokenStore;
use overseer_life::connectors::{claude, codex, cursor, devin, local_archives, opencode, x};
use overseer_life::snapshot::Status;

const CLAUDE_SECRET: &str = "sk-ant-FIXTURE-SECRET-aa11bb22";
const CODEX_SECRET: &str = "codex-FIXTURE-SECRET-cc33dd44";
const CURSOR_SECRET: &str = "cursor-FIXTURE-SECRET-ee55ff66";
const DEVIN_SECRET: &str = "devin-FIXTURE-SECRET-00778899";
const OPENCODE_SECRET: &str = "opencode-FIXTURE-SECRET-11223344";
const X_SECRET: &str = "x-FIXTURE-SECRET-55667788";

fn json_env(server: &MockServer, key: &str, path: &str) -> (String, String) {
    (
        format!("OVS_LIFE_URL_{key}"),
        format!("{}{path}", server.base),
    )
}

fn claude_home(home: &TestHome, expires_ms: i64) {
    home.write(
        ".claude/.credentials.json",
        &format!(
            r#"{{"claudeAiOauth":{{"accessToken":"{CLAUDE_SECRET}","refreshToken":"rt-fixture","expiresAt":{expires_ms},"subscriptionType":"max","rateLimitTier":"default_claude_max_5x","scopes":["user:profile"]}}}}"#
        ),
    );
}

fn codex_home(home: &TestHome, access_token: &str) {
    home.write(
        ".codex/auth.json",
        &format!(
            r#"{{"tokens":{{"access_token":"{access_token}","refresh_token":"rt-fixture","account_id":"acct-1"}},"last_refresh":"2027-01-01T00:00:00Z"}}"#
        ),
    );
}

fn jwt_with_exp(exp_s: i64) -> String {
    fn b64(data: &str) -> String {
        overseer_life::creds::base64url_encode(data.as_bytes())
    }
    format!(
        "{}.{}.sig",
        b64("{\"alg\":\"HS256\"}"),
        b64(&format!("{{\"exp\":{exp_s}}}"))
    )
}

// ---------------------------------------------------------------------
// Claude
// ---------------------------------------------------------------------

#[test]
fn claude_ok() {
    let server = MockServer::start();
    server.route(
        "/usage",
        200,
        r#"{"five_hour":{"utilization":42.0,"resets_at":"2027-01-15T06:00:00Z"},"seven_day":{"utilization":10.0,"resets_at":"2027-01-20T00:00:00Z"},"limits":[],"extra_usage":{"is_enabled":true,"used_credits":250,"monthly_limit":1000}}"#,
    );
    let home = TestHome::new("claude-ok");
    claude_home(&home, 9_999_999_999_999);
    let (k, v) = json_env(&server, "CLAUDE_USAGE", "/usage");
    let ctx = test_ctx(&home, &[(k.as_str(), v.as_str())], FakeRunner::new());
    let snap = claude::ClaudeConnector.fetch(&ctx);
    assert!(matches!(snap.status, Status::Ok), "{snap:?}");
    assert_eq!(snap.plan.as_deref(), Some("Max (5x)"));
    let five_h = snap.limits.iter().find(|l| l.name == "5h").unwrap();
    assert_eq!(five_h.used_percent, Some(42.0));
    assert_eq!(server.requests().len(), 1);
}

#[test]
fn claude_needs_auth_no_files() {
    let home = TestHome::new("claude-none");
    let ctx = test_ctx(&home, &[], FakeRunner::new());
    let snap = claude::ClaudeConnector.fetch(&ctx);
    assert!(matches!(snap.status, Status::NeedsAuth { .. }));
}

#[test]
fn claude_expired_makes_no_request() {
    let server = MockServer::start();
    server.route("/usage", 200, "{}");
    let home = TestHome::new("claude-exp");
    // expiresAt well before ctx.now_ms (1_800_000_000_000).
    claude_home(&home, 1_000_000);
    let (k, v) = json_env(&server, "CLAUDE_USAGE", "/usage");
    let ctx = test_ctx(&home, &[(k.as_str(), v.as_str())], FakeRunner::new());
    let snap = claude::ClaudeConnector.fetch(&ctx);
    assert!(matches!(snap.status, Status::Expired), "{snap:?}");
    assert!(server.requests().is_empty(), "expired token must not fetch");
    // And certainly nothing hit any token endpoint.
    assert!(!server.hit("token"));
}

#[test]
fn claude_401_needs_auth() {
    let server = MockServer::start();
    server.route("/usage", 401, r#"{"error":"unauthorized"}"#);
    let home = TestHome::new("claude-401");
    claude_home(&home, 9_999_999_999_999);
    let (k, v) = json_env(&server, "CLAUDE_USAGE", "/usage");
    let ctx = test_ctx(&home, &[(k.as_str(), v.as_str())], FakeRunner::new());
    let snap = claude::ClaudeConnector.fetch(&ctx);
    assert!(matches!(snap.status, Status::NeedsAuth { .. }), "{snap:?}");
}

#[test]
fn claude_malformed_json() {
    let server = MockServer::start();
    server.route("/usage", 200, "this is not json");
    let home = TestHome::new("claude-bad");
    claude_home(&home, 9_999_999_999_999);
    let (k, v) = json_env(&server, "CLAUDE_USAGE", "/usage");
    let ctx = test_ctx(&home, &[(k.as_str(), v.as_str())], FakeRunner::new());
    let snap = claude::ClaudeConnector.fetch(&ctx);
    assert!(matches!(snap.status, Status::Error { .. }), "{snap:?}");
}

#[test]
fn claude_oversized_response_rejected() {
    let server = MockServer::start();
    server.route("/usage", 200, &"x".repeat(4096));
    let home = TestHome::new("claude-big");
    claude_home(&home, 9_999_999_999_999);
    let (k, v) = json_env(&server, "CLAUDE_USAGE", "/usage");
    let mut ctx = test_ctx(&home, &[(k.as_str(), v.as_str())], FakeRunner::new());
    ctx.http.max_response_bytes = 128;
    let snap = claude::ClaudeConnector.fetch(&ctx);
    assert!(matches!(snap.status, Status::Error { .. }), "{snap:?}");
}

#[test]
fn claude_keychain_is_attribute_only_without_opt_in() {
    let home = TestHome::new("claude-kc");
    // Pretend the item exists; a secret read would need -w.
    let runner = FakeRunner::new().respond("find-generic-password", 0, "item-exists-attrs");
    let calls = runner.calls_handle();
    let ctx = test_ctx(&home, &[], runner);
    let found = claude::ClaudeConnector.discover(&ctx);
    let kc = found.iter().find(|d| d.kind == "keychain").unwrap();
    assert!(kc.present);
    // Existence checks never pass -w.
    for (prog, args) in calls.lock().unwrap().iter() {
        assert_eq!(prog, "/usr/bin/security");
        assert!(!args.contains(&"-w".to_string()), "{args:?}");
        assert!(!args.contains(&"-g".to_string()));
    }
    // fetch must not attempt the -w secret read without opt-in either.
    let runner = FakeRunner::new().respond("find-generic-password -w", 0, CLAUDE_SECRET);
    let calls = runner.calls_handle();
    let ctx = test_ctx(&home, &[], runner);
    let snap = claude::ClaudeConnector.fetch(&ctx);
    assert!(matches!(snap.status, Status::NeedsAuth { .. }));
    for (_, args) in calls.lock().unwrap().iter() {
        assert!(!args.contains(&"-w".to_string()), "{args:?}");
    }
}

// ---------------------------------------------------------------------
// Codex
// ---------------------------------------------------------------------

#[test]
fn codex_ok() {
    let server = MockServer::start();
    server.route_with_headers(
        "/wham/usage",
        200,
        r#"{"rate_limit":{"primary_window":{"used_percent":30,"reset_at":1890000000,"limit_window_seconds":18000},"secondary_window":{"used_percent":5,"limit_window_seconds":604800}},"credits":{"has_credits":true,"balance":500},"plan_type":"plus"}"#,
        &[("x-codex-credits-balance", "500")],
    );
    let home = TestHome::new("codex-ok");
    codex_home(&home, &jwt_with_exp(1_900_000_000));
    let (k, v) = json_env(&server, "CODEX_USAGE", "/wham/usage");
    let ctx = test_ctx(&home, &[(k.as_str(), v.as_str())], FakeRunner::new());
    let snap = codex::CodexConnector.fetch(&ctx);
    assert!(matches!(snap.status, Status::Ok), "{snap:?}");
    assert_eq!(snap.plan.as_deref(), Some("Plus"));
    assert!(snap.limits.iter().any(|l| l.name == "5h"));
}

#[test]
fn codex_expired_jwt_no_request() {
    let server = MockServer::start();
    server.route("/wham/usage", 200, "{}");
    let home = TestHome::new("codex-exp");
    codex_home(&home, &jwt_with_exp(1_000)); // expired long ago
    let (k, v) = json_env(&server, "CODEX_USAGE", "/wham/usage");
    let ctx = test_ctx(&home, &[(k.as_str(), v.as_str())], FakeRunner::new());
    let snap = codex::CodexConnector.fetch(&ctx);
    assert!(matches!(snap.status, Status::Expired), "{snap:?}");
    assert!(server.requests().is_empty());
}

#[test]
fn codex_api_key_only_unavailable() {
    let home = TestHome::new("codex-key");
    home.write(".codex/auth.json", r#"{"OPENAI_API_KEY":"sk-fixture-key"}"#);
    let ctx = test_ctx(&home, &[], FakeRunner::new());
    let snap = codex::CodexConnector.fetch(&ctx);
    assert!(
        matches!(snap.status, Status::Unavailable { .. }),
        "{snap:?}"
    );
}

#[test]
fn codex_needs_auth_and_401() {
    let home = TestHome::new("codex-none");
    let ctx = test_ctx(&home, &[], FakeRunner::new());
    assert!(matches!(
        codex::CodexConnector.fetch(&ctx).status,
        Status::NeedsAuth { .. }
    ));

    let server = MockServer::start();
    server.route("/wham/usage", 401, "{}");
    let home = TestHome::new("codex-401");
    codex_home(&home, &jwt_with_exp(1_900_000_000));
    let (k, v) = json_env(&server, "CODEX_USAGE", "/wham/usage");
    let ctx = test_ctx(&home, &[(k.as_str(), v.as_str())], FakeRunner::new());
    let snap = codex::CodexConnector.fetch(&ctx);
    assert!(matches!(snap.status, Status::NeedsAuth { .. }), "{snap:?}");
}

#[test]
fn http_429_maps_to_rate_limited() {
    // With Retry-After the hint is carried into the message.
    let server = MockServer::start();
    server.route_with_headers("/wham/usage", 429, "{}", &[("retry-after", "30")]);
    let home = TestHome::new("cx-429");
    codex_home(&home, &jwt_with_exp(1_900_000_000));
    let (k, v) = json_env(&server, "CODEX_USAGE", "/wham/usage");
    let ctx = test_ctx(&home, &[(k.as_str(), v.as_str())], FakeRunner::new());
    let snap = codex::CodexConnector.fetch(&ctx);
    let Status::Error { kind, message } = &snap.status else {
        panic!("expected error, got {snap:?}");
    };
    assert_eq!(*kind, overseer_life::ErrorKind::RateLimited);
    assert_eq!(message, "rate limited; retry after 30 s");

    // Without the header the message is just "rate limited".
    let server = MockServer::start();
    server.route("/wham/usage", 429, "{}");
    let home = TestHome::new("cx-429b");
    codex_home(&home, &jwt_with_exp(1_900_000_000));
    let (k, v) = json_env(&server, "CODEX_USAGE", "/wham/usage");
    let ctx = test_ctx(&home, &[(k.as_str(), v.as_str())], FakeRunner::new());
    let snap = codex::CodexConnector.fetch(&ctx);
    let Status::Error { kind, message } = &snap.status else {
        panic!("expected error, got {snap:?}");
    };
    assert_eq!(*kind, overseer_life::ErrorKind::RateLimited);
    assert_eq!(message, "rate limited");
}

// ---------------------------------------------------------------------
// Cursor
// ---------------------------------------------------------------------

fn cursor_home(home: &TestHome, token: &str) -> FakeRunner {
    home.write(
        "Library/Application Support/Cursor/User/globalStorage/state.vscdb",
        "not-a-real-db",
    );
    FakeRunner::new().respond(
        "sqlite3",
        0,
        &format!(
            r#"[{{"key":"cursorAuth/accessToken","value":"{token}"}},{{"key":"cursorAuth/stripeMembershipType","value":"pro"}}]"#
        ),
    )
}

#[test]
fn cursor_ok_readonly_sqlite() {
    let server = MockServer::start();
    server.route(
        "/usage",
        200,
        r#"{"planUsage":{"totalPercentUsed":61.5},"billingCycleEnd":1900000000000,"spendLimitUsage":{"individualLimit":5000,"individualRemaining":2000}}"#,
    );
    server.route(
        "/credits",
        200,
        r#"{"hasCreditGrants":true,"totalCents":1000,"usedCents":250}"#,
    );
    let home = TestHome::new("cursor-ok");
    let runner = cursor_home(&home, CURSOR_SECRET);
    let (k1, v1) = json_env(&server, "CURSOR_USAGE", "/usage");
    let (k2, v2) = json_env(&server, "CURSOR_CREDITS", "/credits");
    let ctx = test_ctx(
        &home,
        &[(k1.as_str(), v1.as_str()), (k2.as_str(), v2.as_str())],
        runner,
    );
    let snap = cursor::CursorConnector.fetch(&ctx);
    assert!(matches!(snap.status, Status::Ok), "{snap:?}");
    assert_eq!(snap.plan.as_deref(), Some("Pro"));
    let cur = snap.limits.iter().find(|l| l.name == "Current").unwrap();
    assert_eq!(cur.used_percent, Some(61.5));
    // R2: the sqlite open was read-only — no -wal/-shm siblings.
    assert!(!home
        .wal_or_shm_created("Library/Application Support/Cursor/User/globalStorage/state.vscdb"));
}

#[test]
fn cursor_needs_auth_no_db() {
    let home = TestHome::new("cursor-none");
    let ctx = test_ctx(&home, &[], FakeRunner::new());
    assert!(matches!(
        cursor::CursorConnector.fetch(&ctx).status,
        Status::NeedsAuth { .. }
    ));
}

#[test]
fn cursor_expired_jwt() {
    let home = TestHome::new("cursor-exp");
    let runner = cursor_home(&home, &jwt_with_exp(1_000));
    let ctx = test_ctx(&home, &[], runner);
    assert!(matches!(
        cursor::CursorConnector.fetch(&ctx).status,
        Status::Expired
    ));
}

#[test]
fn cursor_401() {
    let server = MockServer::start();
    server.route("/usage", 401, "{}");
    let home = TestHome::new("cursor-401");
    let runner = cursor_home(&home, CURSOR_SECRET);
    let (k, v) = json_env(&server, "CURSOR_USAGE", "/usage");
    let ctx = test_ctx(&home, &[(k.as_str(), v.as_str())], runner);
    assert!(matches!(
        cursor::CursorConnector.fetch(&ctx).status,
        Status::NeedsAuth { .. }
    ));
}

// ---------------------------------------------------------------------
// Devin
// ---------------------------------------------------------------------

#[test]
fn devin_ok() {
    let server = MockServer::start();
    server.route(
        "/exa.seat_management_pb.SeatManagementService/GetUserStatus",
        200,
        r#"{"userStatus":{"planStatus":{"dailyQuotaRemainingPercent":80,"weeklyQuotaRemainingPercent":40,"usedPromptCredits":12.5,"availablePromptCredits":87.5,"planEnd":1900000000,"planInfo":{"planName":"acme_team"}}}}"#,
    );
    let home = TestHome::new("devin-ok");
    home.write(
        ".local/share/devin/credentials.toml",
        &format!(
            "windsurf_api_key = \"{DEVIN_SECRET}\"\napi_server_url = \"{}\"\n",
            server.base
        ),
    );
    let ctx = test_ctx(&home, &[], FakeRunner::new());
    let snap = devin::DevinConnector.fetch(&ctx);
    assert!(matches!(snap.status, Status::Ok), "{snap:?}");
    assert_eq!(snap.plan.as_deref(), Some("Acme Team"));
    let daily = snap.limits.iter().find(|l| l.name == "Daily").unwrap();
    assert_eq!(daily.used_percent, Some(20.0));
}

#[test]
fn devin_needs_auth_and_401() {
    let home = TestHome::new("devin-none");
    let ctx = test_ctx(&home, &[], FakeRunner::new());
    assert!(matches!(
        devin::DevinConnector.fetch(&ctx).status,
        Status::NeedsAuth { .. }
    ));

    let server = MockServer::start();
    server.route("/exa.", 401, "{}");
    let home = TestHome::new("devin-401");
    home.write(
        ".local/share/devin/credentials.toml",
        &format!(
            "windsurf_api_key = \"{DEVIN_SECRET}\"\napi_server_url = \"{}\"\n",
            server.base
        ),
    );
    let ctx = test_ctx(&home, &[], FakeRunner::new());
    assert!(matches!(
        devin::DevinConnector.fetch(&ctx).status,
        Status::NeedsAuth { .. }
    ));
}

#[test]
fn devin_rejects_non_loopback_http() {
    let home = TestHome::new("devin-insec");
    home.write(
        ".local/share/devin/credentials.toml",
        "windsurf_api_key = \"x\"\napi_server_url = \"http://evil.example.com\"\n",
    );
    let ctx = test_ctx(&home, &[], FakeRunner::new());
    assert!(matches!(
        devin::DevinConnector.fetch(&ctx).status,
        Status::Error { .. }
    ));
}

// ---------------------------------------------------------------------
// OpenCode
// ---------------------------------------------------------------------

#[test]
fn opencode_ok_401_403_and_signed_in() {
    let server = MockServer::start();
    server.route(
        "/zen/go/v1/usage",
        200,
        r#"{"usage":{"rolling":{"percent":12.5,"resetsAt":"2027-01-15T10:00:00Z"},"weekly":{"percent":3.0},"monthly":{"percent":1.0}}}"#,
    );
    let home = TestHome::new("oc-ok");
    home.write(
        ".local/share/opencode/auth.json",
        &format!(r#"{{"opencode-go":{{"type":"api","key":"{OPENCODE_SECRET}"}}}}"#),
    );
    let (k, v) = json_env(&server, "OPENCODE_USAGE", "/zen/go/v1/usage");
    let ctx = test_ctx(&home, &[(k.as_str(), v.as_str())], FakeRunner::new());
    let snap = opencode::OpenCodeConnector.fetch(&ctx);
    assert!(matches!(snap.status, Status::Ok), "{snap:?}");
    assert_eq!(snap.plan.as_deref(), Some("Go"));
    assert_eq!(snap.limits.len(), 3);

    // no auth file → needs_auth
    let home = TestHome::new("oc-none");
    let ctx = test_ctx(&home, &[], FakeRunner::new());
    assert!(matches!(
        opencode::OpenCodeConnector.fetch(&ctx).status,
        Status::NeedsAuth { .. }
    ));

    // auth without go key → ok with a hint line
    let home = TestHome::new("oc-nogo");
    home.write(
        ".local/share/opencode/auth.json",
        r#"{"anthropic":{"type":"oauth"}}"#,
    );
    let ctx = test_ctx(&home, &[], FakeRunner::new());
    let snap = opencode::OpenCodeConnector.fetch(&ctx);
    assert!(matches!(snap.status, Status::Ok));
    assert_eq!(snap.usage_lines[0].label, "Limits");

    // 401 → needs_auth; 403 → unavailable
    let server = MockServer::start();
    server.route("/zen/go/v1/usage", 401, "{}");
    let home = TestHome::new("oc-401");
    home.write(
        ".local/share/opencode/auth.json",
        &format!(r#"{{"opencode-go":{{"key":"{OPENCODE_SECRET}"}}}}"#),
    );
    let (k, v) = json_env(&server, "OPENCODE_USAGE", "/zen/go/v1/usage");
    let ctx = test_ctx(&home, &[(k.as_str(), v.as_str())], FakeRunner::new());
    assert!(matches!(
        opencode::OpenCodeConnector.fetch(&ctx).status,
        Status::NeedsAuth { .. }
    ));

    let server = MockServer::start();
    server.route("/zen/go/v1/usage", 403, "{}");
    let home = TestHome::new("oc-403");
    home.write(
        ".local/share/opencode/auth.json",
        &format!(r#"{{"opencode-go":{{"key":"{OPENCODE_SECRET}"}}}}"#),
    );
    let (k, v) = json_env(&server, "OPENCODE_USAGE", "/zen/go/v1/usage");
    let ctx = test_ctx(&home, &[(k.as_str(), v.as_str())], FakeRunner::new());
    assert!(matches!(
        opencode::OpenCodeConnector.fetch(&ctx).status,
        Status::Unavailable { .. }
    ));
}

// ---------------------------------------------------------------------
// Local archives
// ---------------------------------------------------------------------

#[test]
fn claude_archive_totals() {
    let home = TestHome::new("cl-arch");
    let ts_recent = "2027-01-14T12:00:00Z"; // ctx.now is 2027-01-15
    let ts_old = "2026-12-20T12:00:00Z"; // within 30d, outside 7d
    home.write(
        ".claude/projects/-Users-test-proj/aaa.jsonl",
        &format!(
            concat!(
                r#"{{"type":"user","timestamp":"{t1}","sessionId":"s1","uuid":"u0"}}"#,
                "\n",
                r#"{{"type":"assistant","timestamp":"{t1}","sessionId":"s1","requestId":"r1","message":{{"id":"m1","usage":{{"input_tokens":100,"output_tokens":50,"cache_read_input_tokens":25}}}}}}"#,
                "\n",
                // duplicate requestId → deduped
                r#"{{"type":"assistant","timestamp":"{t1}","sessionId":"s1","requestId":"r1","message":{{"id":"m1","usage":{{"input_tokens":100,"output_tokens":50,"cache_read_input_tokens":25}}}}}}"#,
                "\n",
                r#"{{"type":"assistant","timestamp":"{t2}","sessionId":"s2","requestId":"r9","message":{{"id":"m9","usage":{{"input_tokens":10,"output_tokens":10}}}}}}"#,
            ),
            t1 = ts_recent,
            t2 = ts_old
        ),
    );
    let ctx = test_ctx(&home, &[], FakeRunner::new());
    let snap = local_archives::ClaudeArchiveConnector.fetch(&ctx);
    assert!(matches!(snap.status, Status::Ok), "{snap:?}");
    // recent file's 175 tokens land in 24h/7d/30d; old one's 20 in 30d only.
    let get = |name: &str| snap.metrics.iter().find(|m| m.name == name).unwrap().value;
    assert_eq!(get("tokens_24h"), 175.0);
    assert_eq!(get("tokens_7d"), 175.0);
    assert_eq!(get("tokens_30d"), 195.0);
    assert_eq!(get("sessions_30d"), 2.0);
}

#[test]
fn codex_archive_totals() {
    let home = TestHome::new("cx-arch");
    // Codex rollouts: sessions/YYYY/MM/DD/rollout-*.jsonl; the summary is
    // the LAST token_count event per file.
    home.write(
        ".codex/sessions/2027/01/14/rollout-1.jsonl",
        concat!(
            r#"{"type":"event_msg","timestamp":"2027-01-14T12:00:00Z","payload":{"type":"token_count","info":{"total_token_usage":{"total_tokens":1000}}}}"#,
            "\n",
            // The last token_count wins; it also carries the rate-limit windows.
            r#"{"type":"event_msg","timestamp":"2027-01-14T12:30:00Z","payload":{"type":"token_count","info":{"total_token_usage":{"total_tokens":2500}},"rate_limits":{"primary":{"used_percent":12,"window_minutes":300},"secondary":{"used_percent":4,"window_minutes":10080}}}}"#,
            "\n"
        ),
    );
    let ctx = test_ctx(&home, &[], FakeRunner::new());
    let snap = local_archives::CodexArchiveConnector.fetch(&ctx);
    assert!(matches!(snap.status, Status::Ok), "{snap:?}");
    let get = |name: &str| snap.metrics.iter().find(|m| m.name == name).unwrap().value;
    assert_eq!(get("tokens_7d"), 2500.0); // last event wins, not the sum
    assert!(snap.limits.iter().any(|l| l.name == "5h"));
}

#[test]
fn archives_absent() {
    let home = TestHome::new("arch-none");
    let ctx = test_ctx(&home, &[], FakeRunner::new());
    assert!(matches!(
        local_archives::ClaudeArchiveConnector.fetch(&ctx).status,
        Status::Unavailable { .. }
    ));
    assert!(matches!(
        local_archives::CodexArchiveConnector.fetch(&ctx).status,
        Status::Unavailable { .. }
    ));
}

// ---------------------------------------------------------------------
// X
// ---------------------------------------------------------------------

/// Tokens ride the in-memory store only — Phase 0 reads no file and no
/// env var for X credentials.
fn x_store() -> x::MemoryTokenStore {
    let store = x::MemoryTokenStore::default();
    store.save(&x::XTokens {
        access_token: X_SECRET.into(),
        refresh_token: None,
        expires_at_ms: None,
        scope: None,
    });
    store
}

fn x_ctx(home: &TestHome, server: &MockServer, cap_usd: f64) -> overseer_life::Ctx {
    let base = server.base.clone();
    let cap = cap_usd.to_string();
    test_ctx(
        home,
        &[
            ("OVS_LIFE_URL_X_API", base.as_str()),
            ("OVS_LIFE_X_DAILY_CAP_USD", cap.as_str()),
        ],
        FakeRunner::new(),
    )
}

#[test]
fn x_counts_ok() {
    let server = MockServer::start();
    server.route(
        "/2/users/me",
        200,
        r#"{"data":{"id":"1","username":"tester","public_metrics":{"followers_count":1234,"following_count":567,"tweet_count":890}}}"#,
    );
    let home = TestHome::new("x-ok");
    let ctx = x_ctx(&home, &server, 5.0);
    let snap = x::XConnector::with_store(Box::new(x_store())).fetch(&ctx);
    assert!(matches!(snap.status, Status::Ok), "{snap:?}");
    assert_eq!(snap.account.id.as_deref(), Some("1"));
    assert_eq!(snap.account.label.as_deref(), Some("@tester"));
    let followers = snap.metrics.iter().find(|m| m.name == "followers").unwrap();
    assert_eq!(followers.value, 1234.0);
    assert_eq!(server.requests().len(), 1);
}

#[test]
fn x_needs_auth_memory_only() {
    let home = TestHome::new("x-none");
    let ctx = test_ctx(&home, &[], FakeRunner::new());
    let connector = x::XConnector::new();
    let snap = connector.fetch(&ctx);
    let Status::NeedsAuth { hint } = &snap.status else {
        panic!("expected needs_auth, got {snap:?}");
    };
    assert!(hint.contains("x_spike.rs"), "{hint}");
    // Nothing on disk or in the environment is reported as a credential.
    assert!(connector.discover(&ctx).is_empty());
}

#[test]
fn x_expired_token_no_request() {
    let server = MockServer::start();
    server.route("/2/users/me", 200, "{}");
    let home = TestHome::new("x-exp");
    let store = x::MemoryTokenStore::default();
    store.save(&x::XTokens {
        access_token: X_SECRET.into(),
        refresh_token: None,
        expires_at_ms: Some(1_000), // long expired vs ctx.now
        scope: None,
    });
    let ctx = test_ctx(
        &home,
        &[("OVS_LIFE_URL_X_API", server.base.as_str())],
        FakeRunner::new(),
    );
    let snap = x::XConnector::with_store(Box::new(store)).fetch(&ctx);
    assert!(matches!(snap.status, Status::Expired), "{snap:?}");
    assert!(server.requests().is_empty());
}

#[test]
fn x_new_followers_stops_at_known_id() {
    let server = MockServer::start();
    server.route(
        "/2/users/42/followers",
        200,
        // first page: 2 new + 1 known → stop, never fetch page 2
        r#"{"data":[{"id":"n1"},{"id":"n2"},{"id":"k1"},{"id":"SHOULD-NOT-SEE"}],"meta":{"next_token":"p2"}}"#,
    );
    let home = TestHome::new("x-inc");
    let ctx = x_ctx(&home, &server, 500.0);
    let ledger = x::CostLedger::new(u64::MAX, ctx.now_ms);
    let store = x_store();
    let pricing = x::XPricing::default();
    let known: HashSet<String> = ["k1".to_string()].into_iter().collect();
    let scan = x::new_followers(&ctx, &store, &ledger, &pricing, "42", &known).unwrap();
    assert!(scan.hit_known);
    assert_eq!(scan.ids, vec!["n1", "n2"]);
    assert_eq!(scan.pages, 1);
    assert_eq!(server.requests().len(), 1);
}

#[test]
fn x_full_scan_pages() {
    let server = MockServer::start();
    // Route ordering matters: page with pagination_token is a prefix of
    // the base path, so register the more specific prefix FIRST? No —
    // the mock matches first route whose prefix the path starts with.
    // "/2/users/7/followers?max_results=1000" vs "&pagination_token" —
    // register token-ful page first.
    server.route(
        "/2/users/7/followers?max_results=1000&user.fields=username&pagination_token=page2",
        200,
        r#"{"data":[{"id":"c"}],"meta":{}}"#,
    );
    server.route(
        "/2/users/7/followers",
        200,
        r#"{"data":[{"id":"a"},{"id":"b"}],"meta":{"next_token":"page2"}}"#,
    );
    let home = TestHome::new("x-full");
    let ctx = x_ctx(&home, &server, 500.0);
    let ledger = x::CostLedger::new(u64::MAX, ctx.now_ms);
    let store = x_store();
    let pricing = x::XPricing::default();
    let scan = x::full_follower_scan(&ctx, &store, &ledger, &pricing, "7").unwrap();
    assert_eq!(scan.ids, vec!["a", "b", "c"]);
    assert_eq!(scan.pages, 2);
}

#[test]
fn x_budget_refusal_sends_nothing() {
    let server = MockServer::start();
    server.route("/2/users/9/followers", 200, r#"{"data":[],"meta":{}}"#);
    let home = TestHome::new("x-cap");
    // Cap of $0.001 — a followers page at worst-case 1000 resources ×
    // $0.010 = $10 refuses before any request.
    let ctx = x_ctx(&home, &server, 0.001);
    let ledger = x::CostLedger::new(1_000, ctx.now_ms); // $0.001
    let store = x_store();
    let pricing = x::XPricing::default();
    let err = x::full_follower_scan(&ctx, &store, &ledger, &pricing, "9").unwrap_err();
    assert!(matches!(err, x::XError::BudgetExceeded { .. }));
    assert!(
        server.requests().is_empty(),
        "budget refusal must not fetch"
    );
}

#[test]
fn x_pricing_classes() {
    let none = x::XPricing::default();
    // /2/users/me is a plain User: Read — never owned.
    assert_eq!(
        x::CostLedger::estimate("/2/users/me?user.fields=public_metrics", 1, &none),
        x::USER_READ_MICRO_USD
    );
    // Even with an owned app and a known self id, /2/users/me stays
    // User: Read.
    let owned = x::XPricing {
        self_id: Some("42".into()),
        app_owned_by_user: true,
    };
    assert_eq!(
        x::CostLedger::estimate("/2/users/me", 1, &owned),
        x::USER_READ_MICRO_USD
    );
    // Self followers + owned app → owned read.
    assert_eq!(
        x::CostLedger::estimate("/2/users/42/followers", 20, &owned),
        x::OWNED_READ_MICRO_USD * 20
    );
    // Self followers without app ownership → public user read.
    let not_owned = x::XPricing {
        self_id: Some("42".into()),
        app_owned_by_user: false,
    };
    assert_eq!(
        x::CostLedger::estimate("/2/users/42/followers", 20, &not_owned),
        x::USER_READ_MICRO_USD * 20
    );
    // Another user's followers are never owned-priced.
    assert_eq!(
        x::CostLedger::estimate("/2/users/7/followers", 20, &owned),
        x::USER_READ_MICRO_USD * 20
    );
    // Posts bill at the post rate.
    assert_eq!(
        x::CostLedger::estimate("/2/users/7/tweets", 10, &not_owned),
        x::POST_READ_MICRO_USD * 10
    );
}

#[test]
fn x_cost_ledger_day_rollover() {
    let day0 = 1_800_000_000_000i64;
    let day1 = day0 + 86_400_000;
    let pricing = x::XPricing::default();
    let ledger = x::CostLedger::new(10_000, day0); // $0.010 cap
    assert!(ledger.charge(day0, "/2/users/me", 1, &pricing).is_ok()); // $0.010 user read
    assert!(ledger
        .charge(day0, "/2/users/1/followers", 1, &pricing)
        .is_err()); // would exceed
                    // Next UTC day resets the ledger.
    assert!(ledger
        .charge(day1, "/2/users/1/followers", 1, &pricing)
        .is_ok());
    assert_eq!(ledger.day(), overseer_life::time::utc_day(day1));
}

#[test]
fn x_oauth_callback_parsing() {
    // Happy path with percent-decoding.
    assert_eq!(
        x::parse_oauth_callback("/callback?code=a%2Fb%3Dc&state=s1", "s1").unwrap(),
        "a/b=c"
    );
    // State mismatch is rejected.
    assert!(x::parse_oauth_callback("/callback?code=x&state=wrong", "s1").is_err());
    // Missing code is an error even with the right state.
    assert!(x::parse_oauth_callback("/callback?state=s1", "s1").is_err());
    // A different path is not a callback.
    assert!(x::parse_oauth_callback("/favicon.ico", "s1").is_err());
}

#[test]
fn x_pkce_url_shape() {
    let pkce = x::PkcePair {
        verifier: "v".into(),
        challenge: "c".into(),
        state: "s".into(),
    };
    let url = x::authorize_url("client-1", "http://localhost:8080/callback", &pkce);
    assert!(url.starts_with("https://x.com/i/oauth2/authorize?"));
    for needle in [
        "response_type=code",
        "client_id=client-1",
        "code_challenge=c",
        "code_challenge_method=S256",
        "state=s",
    ] {
        assert!(url.contains(needle), "{url} missing {needle}");
    }
    assert!(url.contains("tweet.read%20users.read%20follows.read%20offline.access"));
}

#[test]
fn x_revoke_posts_token_and_client_id() {
    let server = MockServer::start();
    server.route("/2/oauth2/revoke", 200, "{}");
    let home = TestHome::new("x-revoke");
    let ctx = test_ctx(
        &home,
        &[("OVS_LIFE_URL_X_API", server.base.as_str())],
        FakeRunner::new(),
    );
    x::revoke_token(&ctx, "client-1", X_SECRET);
    let reqs = server.requests();
    assert_eq!(reqs.len(), 1, "{reqs:?}");
    assert!(reqs[0].starts_with("POST /2/oauth2/revoke "), "{reqs:?}");
    let raw = server.raw();
    // Fixture secret appears here only inside the form body on the wire —
    // never in a snapshot or error string.
    assert!(raw[0].contains(&format!("token={X_SECRET}")), "{}", raw[0]);
    assert!(raw[0].contains("client_id=client-1"), "{}", raw[0]);
}

#[test]
fn x_spike_scopes_omit_offline_access() {
    let pkce = x::PkcePair {
        verifier: "v".into(),
        challenge: "c".into(),
        state: "s".into(),
    };
    // The spike revokes and discards its token — no refresh grant.
    let url = x::authorize_url_with_scopes(
        "client-1",
        "http://127.0.0.1:8723/callback",
        &pkce,
        &["tweet.read", "users.read", "follows.read"],
    );
    assert!(!url.contains("offline.access"), "{url}");
    assert!(url.contains("scope=tweet.read%20users.read%20follows.read"));
    // The default helper still requests refresh-capable scopes.
    let url = x::authorize_url("client-1", "http://127.0.0.1:8723/callback", &pkce);
    assert!(url.contains("offline.access"));
}

#[test]
fn x_token_exchange_posts_form() {
    let server = MockServer::start();
    server.route(
        "/2/oauth2/token",
        200,
        r#"{"access_token":"at-1","refresh_token":"rt-1","expires_in":7200,"scope":"tweet.read"}"#,
    );
    let home = TestHome::new("x-exch");
    let token_url = format!("{}/2/oauth2/token", server.base);
    let ctx = test_ctx(
        &home,
        &[("OVS_LIFE_URL_X_TOKEN", token_url.as_str())],
        FakeRunner::new(),
    );
    let tokens =
        x::exchange_code(&ctx, "client-1", "http://localhost/cb", "code-1", "ver-1").unwrap();
    assert_eq!(tokens.access_token, "at-1");
    assert_eq!(tokens.refresh_token.as_deref(), Some("rt-1"));
    assert!(tokens.expires_at_ms.unwrap() > ctx.now_ms);
}

// ---------------------------------------------------------------------
// Redaction: secrets never appear in serialized output or error strings
// ---------------------------------------------------------------------

#[test]
fn secrets_never_appear_in_output() {
    let server = MockServer::start();
    server.route("/usage", 500, "server exploded");
    let home = TestHome::new("redact");
    claude_home(&home, 9_999_999_999_999);
    let (k, v) = json_env(&server, "CLAUDE_USAGE", "/usage");
    let ctx = test_ctx(&home, &[(k.as_str(), v.as_str())], FakeRunner::new());
    let snap = claude::ClaudeConnector.fetch(&ctx);
    let serialized = serde_json::to_string(&snap).unwrap();
    for secret in [
        CLAUDE_SECRET,
        CODEX_SECRET,
        CURSOR_SECRET,
        DEVIN_SECRET,
        OPENCODE_SECRET,
        X_SECRET,
    ] {
        assert!(!serialized.contains(secret), "secret leaked in snapshot");
    }
    // error message too
    if let Status::Error { message, .. } = &snap.status {
        assert!(!message.contains(CLAUDE_SECRET));
    }

    // And on success the fingerprint is 18 base64url chars, not the token.
    let server = MockServer::start();
    server.route(
        "/usage",
        200,
        r#"{"five_hour":{"utilization":1.0},"limits":[]}"#,
    );
    let (k, v) = json_env(&server, "CLAUDE_USAGE", "/usage");
    let ctx = test_ctx(&home, &[(k.as_str(), v.as_str())], FakeRunner::new());
    let snap = claude::ClaudeConnector.fetch(&ctx);
    let serialized = serde_json::to_string(&snap).unwrap();
    assert!(!serialized.contains(CLAUDE_SECRET));
    let fp = snap.account.fingerprint.unwrap();
    assert_eq!(fp.len(), 18);
    assert_ne!(fp, CLAUDE_SECRET);
}
