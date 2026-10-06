//! Audit: overseer-life connectors, HTTP layer and the X OAuth/cost code,
//! driven through the public API with the crate's own loopback mocks.
//!
//! Tests marked `#[ignore = "audit: ..."]` are findings: they fail on the
//! audited base commit and document the expected behaviour.

#[allow(dead_code)]
mod common;

use std::io::{Read, Write};
use std::net::TcpListener;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::time::{Duration, Instant};

use common::{test_ctx, FakeRunner, MockServer, TestHome};
use overseer_life::connector::Connector;
use overseer_life::connectors::x::{self, TokenStore};
use overseer_life::connectors::{claude, codex, devin};
use overseer_life::http::{Http, HttpError, Method, Request};
use overseer_life::snapshot::{Snapshot, Status};

const CLAUDE_SECRET: &str = "sk-ant-AUDIT-FIXTURE-0001";
const CODEX_SECRET_EXP: i64 = 1_900_000_000;
const X_SECRET: &str = "x-AUDIT-FIXTURE-0002";

fn url_env(server: &MockServer, key: &str, path: &str) -> (String, String) {
    (
        format!("OVS_LIFE_URL_{key}"),
        format!("{}{path}", server.base),
    )
}

fn claude_snap(tag: &str, body: &str) -> Snapshot {
    let server = MockServer::start();
    server.route("/usage", 200, body);
    let home = TestHome::new(tag);
    home.write(
        ".claude/.credentials.json",
        &format!(
            r#"{{"claudeAiOauth":{{"accessToken":"{CLAUDE_SECRET}","expiresAt":9999999999999,"subscriptionType":"max"}}}}"#
        ),
    );
    let (k, v) = url_env(&server, "CLAUDE_USAGE", "/usage");
    let ctx = test_ctx(&home, &[(k.as_str(), v.as_str())], FakeRunner::new());
    let snap = claude::ClaudeConnector.fetch(&ctx);
    assert!(!format!("{snap:?}").contains(CLAUDE_SECRET));
    snap
}

fn jwt(exp_s: i64) -> String {
    let b = |s: &str| overseer_life::creds::base64url_encode(s.as_bytes());
    format!(
        "{}.{}.sig",
        b("{\"alg\":\"HS256\"}"),
        b(&format!("{{\"exp\":{exp_s}}}"))
    )
}

fn codex_fetch(tag: &str, body: &str) -> std::thread::Result<Snapshot> {
    let server = MockServer::start();
    server.route("/wham/usage", 200, body);
    let home = TestHome::new(tag);
    home.write(
        ".codex/auth.json",
        &format!(
            r#"{{"tokens":{{"access_token":"{}","refresh_token":"rt-fixture","account_id":"a"}},"last_refresh":"2027-01-01T00:00:00Z"}}"#,
            jwt(CODEX_SECRET_EXP)
        ),
    );
    let (k, v) = url_env(&server, "CODEX_USAGE", "/wham/usage");
    let ctx = test_ctx(&home, &[(k.as_str(), v.as_str())], FakeRunner::new());
    catch_unwind(AssertUnwindSafe(|| codex::CodexConnector.fetch(&ctx)))
}

fn percents_in_range(s: &Snapshot) -> bool {
    s.limits
        .iter()
        .all(|l| l.used_percent.is_none_or(|p| (0.0..=100.0).contains(&p)))
}

// ── hostile connector JSON ──────────────────────────────────────────────

#[test]
fn hostile_json_never_panics_or_reports_garbage() {
    for (i, body) in [
        r#"{"five_hour":"x","seven_day":[1],"limits":{"a":1},"extra_usage":{"used_credits":"lots"}}"#,
        r#"{"five_hour":{"utilization":"NaN","resets_at":12},"seven_day":{"utilization":1e308}}"#,
        r#"{"five_hour":{"utilization":1e400}}"#,
        r#"[]"#,
        r#"null"#,
    ]
    .iter()
    .enumerate()
    {
        let s = claude_snap(&format!("aud-claude-{i}"), body);
        assert!(percents_in_range(&s), "{body}: {s:?}");
    }
    for (i, body) in [
        r#"{"rate_limit":"x","credits":[],"plan_type":5}"#,
        r#"{"rate_limit":{"primary_window":{"used_percent":1e308,"reset_at":"soon","limit_window_seconds":-5}}}"#,
        r#"{"rate_limit":{"primary_window":{"used_percent":"30","reset_after_seconds":-9e18}}}"#,
    ]
    .iter()
    .enumerate()
    {
        let s = codex_fetch(&format!("aud-codex-{i}"), body).expect("codex parser panicked");
        assert!(percents_in_range(&s), "{body}: {s:?}");
    }
    let server = MockServer::start();
    server.route(
        "/exa.seat_management_pb.SeatManagementService/GetUserStatus",
        200,
        r#"{"userStatus":{"planStatus":{"dailyQuotaRemainingPercent":-1e308,"weeklyQuotaRemainingPercent":"x","usedPromptCredits":-5,"availablePromptCredits":1e308,"planEnd":1e300,"planInfo":{"planName":7}}}}"#,
    );
    let home = TestHome::new("aud-devin");
    home.write(
        ".local/share/devin/credentials.toml",
        &format!(
            "windsurf_api_key = \"devin-AUDIT\"\napi_server_url = \"{}\"\n",
            server.base
        ),
    );
    let ctx = test_ctx(&home, &[], FakeRunner::new());
    let s = catch_unwind(AssertUnwindSafe(|| devin::DevinConnector.fetch(&ctx)))
        .expect("devin panicked");
    assert!(percents_in_range(&s), "{s:?}");
}

/// `reset_after_seconds` is upstream data. A huge value saturates the
/// float→i64 cast and the following `now + …` overflows: the connector
/// panics instead of returning a snapshot.
#[test]
#[ignore = "audit: life-codex-reset-overflow"]
fn codex_huge_reset_after_seconds_does_not_panic() {
    let r = codex_fetch(
        "aud-codex-reset",
        r#"{"rate_limit":{"primary_window":{"used_percent":30,"reset_after_seconds":1e300,"limit_window_seconds":18000}},"plan_type":"plus"}"#,
    );
    assert!(
        r.is_ok(),
        "CodexConnector::fetch panicked on reset_after_seconds = 1e300"
    );
}

/// Negative utilisation is not a valid reading; the security note says a
/// bad value is never silently zero. It is clamped to a confident 0 %.
#[test]
#[ignore = "audit: life-negative-usage"]
fn negative_utilization_is_not_reported_as_zero_percent() {
    let s = claude_snap(
        "aud-claude-neg",
        r#"{"five_hour":{"utilization":-50.0,"resets_at":"2027-01-15T06:00:00Z"}}"#,
    );
    let five = s
        .limits
        .iter()
        .find(|l| l.name == "5h")
        .map(|l| l.used_percent);
    assert_ne!(
        five,
        Some(Some(0.0)),
        "utilization -50 was reported as 0 % used: {s:?}"
    );
}

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

/// Follower counts cannot be negative; a hostile/buggy body's -5 is passed
/// through as a metric instead of being dropped.
#[test]
#[ignore = "audit: life-negative-usage"]
fn x_negative_public_metric_is_dropped() {
    let server = MockServer::start();
    server.route(
        "/2/users/me",
        200,
        r#"{"data":{"id":"1","username":"a","public_metrics":{"followers_count":-5,"following_count":3}}}"#,
    );
    let home = TestHome::new("aud-x-neg");
    let base = server.base.clone();
    let ctx = test_ctx(
        &home,
        &[
            ("OVS_LIFE_URL_X_API", base.as_str()),
            ("OVS_LIFE_X_DAILY_CAP_USD", "1"),
        ],
        FakeRunner::new(),
    );
    let s = x::XConnector::with_store(Box::new(x_store())).fetch(&ctx);
    assert!(matches!(s.status, Status::Ok), "{s:?}");
    assert!(!format!("{s:?}").contains(X_SECRET));
    let followers = s
        .metrics
        .iter()
        .find(|m| m.name == "followers")
        .map(|m| m.value);
    assert_eq!(
        followers, None,
        "followers_count -5 reported as {followers:?}"
    );
}

// ── http.rs ─────────────────────────────────────────────────────────────

fn get<'a>(url: &'a str, origin: &'a str) -> Request<'a> {
    Request {
        url,
        method: Method::Get,
        headers: &[],
        body: None,
        allowed_origin: origin,
    }
}

#[test]
fn http_rejects_redirects_foreign_origins_plain_http_and_big_bodies() {
    let target = MockServer::start();
    target.route("/", 200, "{}");
    let server = MockServer::start();
    let loc = format!("{}/stolen", target.base);
    server.route_with_headers("/r", 302, "", &[("location", loc.as_str())]);
    server.route("/big", 200, &format!("\"{}\"", "a".repeat(8192)));
    server.route("/ok", 200, "{\"a\":1}");
    let http = Http::for_test();

    let url = format!("{}/r", server.base);
    let r = http.fetch(&get(&url, &server.base));
    assert!(
        !matches!(&r, Ok(f) if f.status == 200),
        "{:?}",
        r.as_ref().map(|f| f.status)
    );
    assert!(
        target.requests().is_empty(),
        "redirect was followed: {:?}",
        target.requests()
    );

    let url = format!("{}/ok", target.base);
    assert!(matches!(
        http.fetch(&get(&url, &server.base)),
        Err(HttpError::OriginNotAllowed)
    ));
    // Userinfo cannot smuggle a different host past the origin check.
    let sneaky = format!(
        "http://{}@{}/ok",
        server.base.trim_start_matches("http://"),
        target.base.trim_start_matches("http://")
    );
    assert!(http.fetch(&get(&sneaky, &server.base)).is_err());
    assert!(target.requests().is_empty());

    let url = format!("{}/ok", server.base);
    assert!(matches!(
        Http::new().fetch(&get(&url, &server.base)),
        Err(HttpError::NotHttps)
    ));

    let mut small = Http::for_test();
    small.max_response_bytes = 1024;
    let url = format!("{}/big", server.base);
    assert!(
        small.fetch(&get(&url, &server.base)).is_err(),
        "body cap holds"
    );

    let mut budget = Http::for_test();
    budget.max_requests = Some(1);
    let url = format!("{}/ok", server.base);
    assert!(budget.fetch(&get(&url, &server.base)).is_ok());
    assert!(matches!(
        budget.fetch(&get(&url, &server.base)),
        Err(HttpError::BudgetExhausted)
    ));
}

/// The cap holds (see above), but the oversized body is reported as a
/// generic transport failure: `HttpError::TooLarge` is never produced.
#[test]
#[ignore = "audit: life-http-toolarge-kind"]
fn http_oversized_body_is_classified_too_large() {
    let server = MockServer::start();
    server.route("/big", 200, &format!("\"{}\"", "a".repeat(8192)));
    let mut small = Http::for_test();
    small.max_response_bytes = 1024;
    let url = format!("{}/big", server.base);
    let r = small.fetch(&get(&url, &server.base));
    assert!(
        matches!(r, Err(HttpError::TooLarge)),
        "got {:?}",
        r.as_ref().err()
    );
}

#[test]
fn http_slowloris_is_cut_off_by_the_timeout() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        for (n, stream) in listener.incoming().take(2).enumerate() {
            let Ok(mut s) = stream else { break };
            std::thread::spawn(move || {
                let mut buf = [0u8; 4096];
                let _ = s.read(&mut buf);
                if n == 0 {
                    // Headers, then one body byte every 200 ms.
                    let _ = s.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 1000\r\n\r\n");
                }
                for _ in 0..100 {
                    if s.write_all(b" ").is_err() {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(200));
                }
            });
        }
    });
    let mut http = Http::for_test();
    http.timeout = Duration::from_millis(1000);
    for _ in 0..2 {
        let started = Instant::now();
        let r = http.fetch(&get(&format!("{base}/x"), &base));
        assert!(r.is_err());
        assert!(
            started.elapsed() < Duration::from_secs(4),
            "{:?}",
            started.elapsed()
        );
    }
}

// ── X: cost ledger, OAuth callback, PKCE ────────────────────────────────

#[test]
fn x_cost_cap_rolls_over_exactly_at_utc_midnight() {
    let pricing = x::XPricing::default();
    let midnight: i64 = 20_000 * 86_400_000;
    let ledger = x::CostLedger::new(x::USER_READ_MICRO_USD, midnight - 1);
    assert!(ledger
        .charge(midnight - 1, "/2/users/me", 1, &pricing)
        .is_ok());
    assert!(ledger
        .charge(midnight - 1, "/2/users/me", 1, &pricing)
        .is_err());
    assert_eq!(
        ledger.spent_micro_usd(),
        x::USER_READ_MICRO_USD,
        "refusal charges nothing"
    );
    assert!(ledger.charge(midnight, "/2/users/me", 1, &pricing).is_ok());
    assert_eq!(ledger.day(), 20_000);
    // Worst case is priced up front; u32::MAX results cannot overflow.
    let big = x::CostLedger::new(u64::MAX, 0);
    assert!(big
        .charge(0, "/2/users/1/followers", u32::MAX, &pricing)
        .is_ok());
    // Pre-epoch clocks still land on a distinct day.
    let neg = x::CostLedger::new(1, -1);
    assert_eq!(neg.day(), -1);
}

#[test]
fn x_oauth_callback_rejects_csrf_and_bad_routes() {
    let ok = x::parse_oauth_callback("/callback?code=a%2Bb+c&state=s1", "s1");
    assert_eq!(ok.unwrap(), "a+b c");
    for path in [
        "/callback?code=c&state=evil",
        "/callback?code=c",
        "/callback?code=&state=s1",
        "/callback?state=s1",
        "/Callback?code=c&state=s1",
        "/callback/../callback?code=c&state=s1",
        "/x?code=c&state=s1",
        "http://evil/callback?code=c&state=s1",
        "/callback?code=c&state=s1%00",
    ] {
        assert!(x::parse_oauth_callback(path, "s1").is_err(), "{path}");
    }
}

#[test]
fn x_pkce_pairs_are_fresh_and_the_verifier_never_leaves() {
    let a = x::pkce_pair().unwrap();
    let b = x::pkce_pair().unwrap();
    assert_ne!(a.verifier, b.verifier);
    assert_ne!(a.state, b.state);
    assert!(a.verifier.len() >= 43, "RFC 7636 minimum");
    assert_ne!(a.challenge, a.verifier, "S256, not plain");
    let url = x::authorize_url("cid", "http://127.0.0.1:8723/callback", &a);
    assert!(url.starts_with(x::AUTHORIZE_URL));
    assert!(url.contains("code_challenge_method=S256"));
    assert!(url.contains(&a.challenge) && url.contains(&a.state));
    assert!(!url.contains(&a.verifier));
}
