//! Interactive X live spike — one process, three API calls, then revoke.
//!
//!   cargo run -p overseer-life --example x_spike -- \
//!       --client-id <X_CLIENT_ID> [--port 8723] [--max-results 20] \
//!       [--cap-usd 0.10] [--app-owned]
//!
//! OAuth shape verified against docs.x.com (read 2026-10-05):
//! - The developer app's client type is "Native App" = a PUBLIC client:
//!   PKCE (S256) is used and there is NO client secret — the token
//!   request sends `client_id` in the form body instead.
//!   <https://docs.x.com/fundamentals/authentication/oauth-2-0/authorization-code>
//!   <https://docs.x.com/fundamentals/authentication/oauth-2-0/user-access-token>
//! - A loopback redirect `http://127.0.0.1:<port>/callback` is allowed
//!   for local development — 127.0.0.1, not localhost — but ONLY if that
//!   exact URI (port included) is registered as a callback URL on the
//!   app in the Developer Console.
//!   <https://docs.x.com/fundamentals/developer-apps>
//! - Revocation is POST https://api.x.com/2/oauth2/revoke with form
//!   fields `token` + `client_id` (public client → no secret).
//!   <https://docs.x.com/fundamentals/authentication/oauth-2-0/user-access-token>
//!
//! Tokens are held in memory only and revoked before exit; nothing is
//! written to disk. --app-owned asserts the developer app is owned by
//! the authenticating user, which makes self followers price at the
//! owned-read rate ($0.001/resource) in the ledger estimate.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::time::{Duration, Instant};

use overseer_life::connector::Ctx;
use overseer_life::connectors::x::{self, MemoryTokenStore, TokenStore, XError, XPricing};
use overseer_life::http::Http;
use overseer_life::snapshot::Status;

const CALLBACK_TIMEOUT: Duration = Duration::from_secs(300); // 5 minutes
/// Hard cap on the callback's request head — a browser GET line plus
/// headers never comes close.
const MAX_HEAD_BYTES: usize = 16 * 1024;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let opt = |name: &str| -> Option<String> {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1).cloned())
            .or_else(|| {
                let prefix = format!("{name}=");
                args.iter()
                    .find_map(|a| a.strip_prefix(prefix.as_str()).map(String::from))
            })
    };
    let client_id = opt("--client-id").or_else(|| std::env::var("X_CLIENT_ID").ok());
    let Some(client_id) = client_id else {
        eprintln!(
            "usage: x_spike --client-id <id> [--port 8723] [--max-results 20] \\\n  \
             [--cap-usd 0.10] [--app-owned]\n  \
             (or set X_CLIENT_ID)"
        );
        std::process::exit(2);
    };
    let port: u16 = opt("--port").and_then(|v| v.parse().ok()).unwrap_or(8723);
    let max_results: u32 = opt("--max-results")
        .and_then(|v| v.parse().ok())
        .unwrap_or(20)
        .clamp(1, 1000);
    let cap_usd: f64 = opt("--cap-usd")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0.10);
    let app_owned = args.iter().any(|a| a == "--app-owned");

    if let Err(e) = run(&client_id, port, max_results, cap_usd, app_owned) {
        eprintln!("x_spike: {e}");
        std::process::exit(1);
    }
}

fn run(
    client_id: &str,
    port: u16,
    max_results: u32,
    cap_usd: f64,
    app_owned: bool,
) -> Result<(), String> {
    // System ctx: real clock, real env, real HTTP. No request cap — the
    // spike makes exactly 4 calls: exchange, counts, one followers page,
    // revoke.
    let ctx = Ctx::system(Http::new(), false);
    let ledger = x::CostLedger::new((cap_usd * 1_000_000.0).max(0.0) as u64, ctx.now_ms);

    // 1. PKCE pair + CSRF state (fails closed if OS entropy is missing).
    let pkce = x::pkce_pair().map_err(|e| format!("PKCE generation failed: {e}"))?;
    let redirect_uri = format!("http://127.0.0.1:{port}/callback");
    // No `offline.access`: the token is revoked and discarded at the end
    // of this run, so asking for a refresh token would buy nothing.
    let url = x::authorize_url_with_scopes(
        client_id,
        &redirect_uri,
        &pkce,
        &["tweet.read", "users.read", "follows.read"],
    );
    println!("Open this URL and authorize (read-only scopes):\n\n  {url}\n");
    println!("Waiting up to 5 minutes for the callback on {redirect_uri} …");

    // 2. Loopback listener: accept connections until a /callback lands.
    let listener = TcpListener::bind(format!("127.0.0.1:{port}"))
        .map_err(|e| format!("cannot bind 127.0.0.1:{port}: {e}"))?;
    listener
        .set_nonblocking(true)
        .map_err(|e| format!("listener setup failed: {e}"))?;
    let code = wait_for_callback(&listener, &pkce.state)?;

    // 3. Exchange the code — public client: client_id in the body, no
    //    secret (doc-cited at the top of this file).
    let store = MemoryTokenStore::default();
    let tokens = x::exchange_code(&ctx, client_id, &redirect_uri, &code, &pkce.verifier)
        .map_err(|e| describe("token exchange", &e))?;
    store.save(&tokens);

    // Make sure a mid-flight failure still revokes before exiting.
    let result = finish(&ctx, &store, &ledger, max_results, app_owned);

    // 4. Revoke whatever we hold — tokens never leave this process.
    if let Some(t) = store.load() {
        x::revoke_token(&ctx, client_id, &t.access_token);
        println!("Access token revoked.");
    }
    result
}

fn finish(
    ctx: &Ctx,
    store: &MemoryTokenStore,
    ledger: &x::CostLedger,
    max_results: u32,
    app_owned: bool,
) -> Result<(), String> {
    // 5. One counts() call — self id also seeds the pricing context.
    let snap = x::counts(ctx, store, ledger, &XPricing::default())
        .map_err(|e| describe("users/me", &e))?;
    if !matches!(snap.status, Status::Ok) {
        return Err(format!("counts() returned {:?}", snap.status));
    }
    let self_id = snap
        .account
        .id
        .clone()
        .ok_or("users/me response carried no id")?;
    let username = snap.account.label.clone().unwrap_or_else(|| "?".into());
    println!("\nAuthenticated as {username} (id {self_id})");
    for m in &snap.metrics {
        println!("  {}: {}", m.name, m.value);
    }

    // 6. Exactly one followers page for self.
    let pricing = XPricing {
        self_id: Some(self_id.clone()),
        app_owned_by_user: app_owned,
    };
    let (ids, _next) = match x::followers_page(ctx, store, ledger, &pricing, &self_id, max_results)
    {
        Ok(page) => page,
        Err(e) => {
            if matches!(e, XError::BudgetExceeded { .. }) && !app_owned {
                eprintln!(
                    "hint: pass --app-owned if your X account owns this developer app \
                     (owned reads cost $0.001 instead of $0.010)"
                );
            }
            return Err(describe("followers", &e));
        }
    };
    println!(
        "Followers page: {} ids returned (ids not printed)",
        ids.len()
    );

    println!(
        "Ledger estimate this run: ${:.4} (cap ${:.2})",
        ledger.spent_micro_usd() as f64 / 1_000_000.0,
        ledger.cap_micro_usd as f64 / 1_000_000.0,
    );
    println!(
        "Compare this with the debit in the X Developer Console — \
         per-request vs per-resource metering is the open question."
    );
    Ok(())
}

/// Poll the listener until a /callback request arrives (5-minute cap).
/// Non-/callback requests get a 404 so a favicon fetch can't eat the
/// single-shot state.
fn wait_for_callback(listener: &TcpListener, expected_state: &str) -> Result<String, String> {
    let deadline = Instant::now() + CALLBACK_TIMEOUT;
    loop {
        if Instant::now() >= deadline {
            return Err("timed out waiting for the OAuth callback".into());
        }
        match listener.accept() {
            Ok((mut stream, _)) => {
                // BSD semantics: on macOS an accepted socket inherits the
                // listener's O_NONBLOCK — flip it back to blocking before
                // reading or the first read gets WouldBlock.
                let _ = stream.set_nonblocking(false);
                let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
                let mut head = Vec::new();
                let mut byte = [0u8; 1];
                while !head.ends_with(b"\r\n\r\n") && head.len() <= MAX_HEAD_BYTES {
                    match stream.read(&mut byte) {
                        Ok(1) => head.push(byte[0]),
                        _ => break,
                    }
                }
                if head.len() > MAX_HEAD_BYTES {
                    let _ = stream.write_all(
                        b"HTTP/1.1 400 Bad Request\r\nconnection: close\r\n\r\nrequest too large",
                    );
                    continue;
                }
                let head_text = String::from_utf8_lossy(&head).to_string();
                let path = head_text
                    .lines()
                    .next()
                    .and_then(|l| l.split_whitespace().nth(1))
                    .unwrap_or("")
                    .to_string();
                match x::parse_oauth_callback(&path, expected_state) {
                    Ok(code) => {
                        let _ = stream.write_all(
                            b"HTTP/1.1 200 OK\r\ncontent-type: text/html\r\nconnection: close\r\n\r\n\
                              <!doctype html><title>Overseer Life</title>\
                              <p>Authorized - you can close this tab.</p>",
                        );
                        return Ok(code);
                    }
                    Err(XError::Callback(m)) if path.starts_with("/callback") => {
                        // A real callback with bad state/code: answer and
                        // fail the run — do not wait for another shot.
                        let _ = stream.write_all(
                            b"HTTP/1.1 400 Bad Request\r\nconnection: close\r\n\r\nbad callback",
                        );
                        return Err(m);
                    }
                    _ => {
                        let _ = stream
                            .write_all(b"HTTP/1.1 404 Not Found\r\nconnection: close\r\n\r\n");
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => return Err(format!("accept failed: {e}")),
        }
    }
}

fn describe(what: &str, e: &XError) -> String {
    // Error strings never carry tokens — XError::to_status keeps the
    // same redaction discipline as every other connector.
    let status = e.to_status();
    format!("{what} failed: {status:?}")
}
