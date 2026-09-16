//! Bake the workspace git commit into `OVERSEER_GIT_SHA` so every binary is
//! self-identifying in run manifests (playbook Ch.12 §4.5: "harness commit"
//! is a required field of the public reporting standard).

fn main() {
    let sha = std::process::Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".into());
    println!("cargo:rustc-env=OVERSEER_GIT_SHA={sha}");
}
