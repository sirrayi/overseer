//! `overseer mcp list` — what the configured MCP servers offer.

use crate::args;
use crate::VERSION;

/// `overseer mcp list` — what the configured MCP servers actually offer.
///
/// Spawns each configured server (through the same env allowlist the engine
/// uses), handshakes, and prints the tool names it advertises, namespaced the
/// way the model has to call them. A name that would shadow a resident tool
/// is marked skipped, because it is not callable. Per-server failures are
/// printed and the exit code is 1 if any server failed: the whole job of this
/// command is to tell the operator whether their config works.
pub(crate) fn cmd_mcp(argv: &[String]) -> i32 {
    let parsed = match args::parse(argv, &[]) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("overseer mcp: {e}");
            return 2;
        }
    };
    match parsed.positionals().first().copied() {
        Some("list") => {}
        Some(other) => {
            eprintln!("overseer mcp: unknown subcommand '{other}' (did you mean `list`?)");
            return 2;
        }
        None => {
            eprintln!("overseer mcp: expected `list` (usage: overseer mcp list)");
            return 2;
        }
    }
    let Some(path) = overseer_core::mcp_config::default_path() else {
        eprintln!("overseer mcp: no HOME set — cannot locate ~/.overseer/mcp.json");
        return 1;
    };
    let servers = match overseer_core::mcp_config::load(&path) {
        Ok(servers) => servers,
        Err(e) => {
            eprintln!("overseer mcp: {}: {e}", path.display());
            return 1;
        }
    };
    if servers.is_empty() {
        println!("no MCP servers configured ({})", path.display());
        return 0;
    }
    println!("config: {}", path.display());
    let mut failed = 0u32;
    for server in &servers {
        match list_server_tools(server) {
            Ok(names) => {
                println!("{}: {} tool(s)", server.name, names.len());
                for name in names {
                    println!("  {name}");
                }
            }
            Err(e) => {
                eprintln!("{}: {e}", server.name);
                failed += 1;
            }
        }
    }
    if failed > 0 {
        eprintln!("overseer mcp: {failed} server(s) failed");
        1
    } else {
        0
    }
}

/// Spawn one configured server, handshake, list. The naming and collision
/// rules are the engine's (`mcp::namespaced` / `mcp::collides_with_resident`),
/// so this command cannot report a name the registry would refuse.
fn list_server_tools(server: &overseer_core::mcp_config::McpServer) -> Result<Vec<String>, String> {
    let env: Vec<(String, String)> = server
        .env
        .iter()
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    let mut client = overseer_core::mcp::StdioClient::spawn_with_env(
        &server.name,
        &server.command,
        &server.args,
        &env,
    )?;
    client.initialize("overseer", VERSION)?;
    let specs = client.list_tools()?;
    let mut names = Vec::with_capacity(specs.len());
    for spec in specs {
        let namespaced = overseer_core::mcp::namespaced(&server.name, &spec.name);
        if overseer_core::mcp::collides_with_resident(&server.name, &spec.name) {
            names.push(format!("{namespaced}  [skipped: shadows a resident tool]"));
        } else {
            names.push(namespaced);
        }
    }
    Ok(names)
}
