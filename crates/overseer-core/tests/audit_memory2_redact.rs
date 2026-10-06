//! Audit (memory v2): `memory::redact::scrub` against real-shaped secrets
//! and benign fixtures. Every secret below is synthetic and assembled at
//! runtime so no literal token shape lands in the repository.

use overseer_core::memory::redact::scrub;

/// A deterministic alphanumeric body of `n` chars (no real key material).
fn body(n: usize) -> String {
    const A: &[u8] = b"A1b2C3d4E5f6G7h8J9k0LmNpQrStUvWxYz";
    (0..n).map(|i| A[(i * 7 + 3) % A.len()] as char).collect()
}

fn assert_scrubbed(label: &str, secret: &str, line: &str) {
    let out = scrub(line);
    assert!(
        !out.contains(secret),
        "{label}: secret survived scrub\n  in:  {line}\n  out: {out}"
    );
}

// ---- held: shapes the current patterns cover ----

#[test]
fn held_aws_access_key_id_github_classic_slack_pem_openai_anthropic() {
    let aws = format!("AKIA{}", "IOSFODNN7EXAMPLQ");
    assert_scrubbed("aws id", &aws, &format!("key {aws} in use"));
    for p in ["ghp_", "gho_", "ghs_", "ghu_", "ghr_"] {
        let t = format!("{p}{}", body(36));
        assert_scrubbed("github classic", &t, &format!("token {t}"));
    }
    let slack = format!("xoxb-{}-{}", "1234567890", body(24));
    assert_scrubbed("slack bot", &slack, &format!("slack {slack}"));
    let openai = format!("sk-proj-{}", body(48));
    assert_scrubbed("openai", &openai, &format!("OPENAI_API_KEY={openai}"));
    let anthropic = format!("sk-ant-api03-{}", body(80));
    assert_scrubbed("anthropic", &anthropic, &format!("use {anthropic} here"));
    let pem_body = body(64);
    let pem = format!("-----BEGIN RSA PRIVATE KEY-----\n{pem_body}\n-----END RSA PRIVATE KEY-----");
    assert_scrubbed("pem", &pem_body, &pem);
    // GCP service-account JSON: the private key rides as a one-line,
    // `\n`-escaped PEM inside JSON — the PEM rule still catches it.
    let gcp = format!(
        "{{\"type\": \"service_account\", \"private_key\": \"-----BEGIN PRIVATE KEY-----\\n{pem_body}\\n-----END PRIVATE KEY-----\\n\", \"client_email\": \"x@p.iam.gserviceaccount.com\"}}"
    );
    assert_scrubbed("gcp sa json", &pem_body, &gcp);
    let bearer = body(40);
    assert_scrubbed(
        "bearer",
        &bearer,
        &format!("Authorization: Bearer {bearer}"),
    );
    assert_scrubbed(
        "env password",
        "hunter2hunter2",
        "DB_PASSWORD=hunter2hunter2",
    );
}

#[test]
fn held_hashes_uuids_and_base64_fixtures_survive() {
    for fixture in [
        "sha256 e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        "commit 9fceb02d0ae598e95dc970b74767f19372d61af8",
        "id 123e4567-e89b-12d3-a456-426614174000 row",
        "fixture: aGVsbG8gd29ybGQgdGhpcyBpcyBhIGJhc2U2NCBmaXh0dXJlIGJsb2I=",
        "integrity sha512-z4PhNX7vuL3xVChQ1m2AB9Yg5AULVxXcg/SpIdNs6c5H0NE8XYXysP+DGNKHfuwvY7kxvUdBeoGlODJ6+SfaPg==",
        "max_tokens 16384, token budget 4000",
    ] {
        assert_eq!(scrub(fixture), fixture, "false positive on {fixture}");
    }
}

// ---- findings ----

#[test]
fn stripe_secret_and_restricted_keys_are_scrubbed() {
    for p in ["sk_live_", "sk_test_", "rk_live_"] {
        let k = format!("{p}{}", body(24));
        assert_scrubbed("stripe", &k, &format!("charge with {k} today"));
    }
}

#[test]
fn github_fine_grained_pat_is_scrubbed() {
    let t = format!("github_{}_{}_{}", "pat", body(22), body(59));
    assert_scrubbed("github fine-grained", &t, &format!("push with {t}"));
}

#[test]
fn bare_jwt_is_scrubbed() {
    // header.payload.signature, base64url — no "Bearer " prefix.
    let jwt = format!(
        "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiI{}.{}",
        body(30),
        body(43)
    );
    assert_scrubbed("jwt", &jwt, &format!("session cookie {jwt}"));
}

#[test]
fn credentials_in_urls_are_scrubbed() {
    let pw = format!("Pw{}", body(14));
    for url in [
        format!("postgres://app:{pw}@db.internal:5432/prod"),
        format!("https://deploy:{pw}@git.example.com/org/repo.git"),
        format!("redis://:{pw}@cache:6379/0"),
    ] {
        assert_scrubbed("url credential", &pw, &format!("connect to {url}"));
    }
}

#[test]
fn env_lines_named_secret_or_access_key_are_scrubbed() {
    let v = format!("wJalrXUtnFEMI/K7MDENG/{}", body(18));
    for line in [
        format!("AWS_SECRET_ACCESS_KEY={v}"),
        format!("aws_secret_access_key = {v}"),
        format!("STRIPE_SECRET_KEY={v}"),
        format!("SECRET_KEY_BASE={v}"),
        format!("export PRIVATE_KEY={v}"),
    ] {
        assert_scrubbed(".env", &v, &line);
    }
}

#[test]
fn json_and_yaml_quoted_secret_fields_are_scrubbed() {
    let v = format!("Q{}", body(20));
    for line in [
        format!("{{\"password\": \"{v}\"}}"),
        format!("{{\"api_key\": \"{v}\"}}"),
        format!("{{\"client_secret\":\"{v}\"}}"),
        format!("'token': '{v}'"),
    ] {
        assert_scrubbed("json", &v, &line);
    }
}

#[test]
fn slack_webhook_and_app_tokens_are_scrubbed() {
    let secret = body(24);
    let hook = format!(
        "https://hooks.slack.com/services/T0{}/B0{}/{secret}",
        body(7),
        body(7)
    );
    assert_scrubbed("slack webhook", &secret, &format!("post to {hook}"));
    let app = format!("xapp-1-A0{}-{}-{}", body(8), "1234567890", body(40));
    assert_scrubbed("slack app token", &app, &format!("socket mode {app}"));
}

#[test]
fn aws_temporary_access_key_id_is_scrubbed() {
    let k = format!("ASIA{}", "IOSFODNN7EXAMPLQ");
    assert_scrubbed("aws sts id", &k, &format!("sts key {k}"));
}

#[test]
fn kebab_case_words_ending_in_sk_are_not_redacted() {
    // `sk-[A-Za-z0-9_-]{20,}` has no left word boundary: any kebab-case
    // identifier containing `…sk-` followed by 20+ chars is destroyed.
    for fixture in [
        "run the task-runner-configuration-service",
        "mount disk-encryption-at-rest-policy first",
        "pip install flask-sqlalchemy-extensions",
        "see risk-assessment-template-v2.md",
    ] {
        assert_eq!(scrub(fixture), fixture, "false positive on {fixture}");
    }
}
