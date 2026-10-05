//! The `code-mode` feature reaches `overseer-core` through this crate:
//! `--no-default-features` builds a binary whose registry has no `run_code`.

use overseer_core::perm::Policy;
use overseer_core::tools::ToolRegistry;

#[test]
fn run_code_is_advertised_only_with_code_mode() {
    let reg = ToolRegistry::core_in(Policy::allow_all(), std::path::Path::new("."));
    let advertised = reg.specs.iter().any(|s| s.name == "run_code");
    assert_eq!(advertised, cfg!(feature = "code-mode"));
    assert_eq!(reg.reachable("run_code"), cfg!(feature = "code-mode"));
}
