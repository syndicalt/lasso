//! `lasso register` — add lasso's MCP endpoint to the local omp agent config
//! (`~/.omp/agent/mcp.json`). Idempotent: preserves other servers and settings.

use std::path::PathBuf;

/// omp's user-level agent config dir; honors the documented override.
fn agent_dir() -> Result<PathBuf, String> {
    if let Some(dir) = std::env::var_os("PI_CODING_AGENT_DIR") {
        return Ok(PathBuf::from(dir));
    }
    let home = std::env::var_os("HOME").ok_or("HOME not set")?;
    Ok(PathBuf::from(home).join(".omp").join("agent"))
}

fn config_path() -> Result<PathBuf, String> {
    Ok(agent_dir()?.join("mcp.json"))
}

const SCHEMA: &str = "https://raw.githubusercontent.com/can1357/oh-my-pi/main/packages/coding-agent/src/config/mcp-schema.json";

/// Add (or update) the `lasso` MCP server entry. Returns a human message.
pub fn register() -> Result<String, String> {
    edit_config(|root| {
        let servers = root
            .as_object_mut()
            .ok_or("mcp.json: top level must be an object")?
            .entry("mcpServers")
            .or_insert_with(|| serde_json::json!({}));
        let servers = servers
            .as_object_mut()
            .ok_or("mcp.json: mcpServers must be an object")?;
        servers.insert(
            "lasso".to_string(),
            serde_json::json!({
                "type": "http",
                "url": crate::mcp::ENDPOINT,
            }),
        );
        Ok(())
    })
}

/// Remove the `lasso` entry (`lasso register --remove`).
pub fn unregister() -> Result<String, String> {
    edit_config(|root| {
        if let Some(servers) = root.get_mut("mcpServers").and_then(|v| v.as_object_mut()) {
            servers.remove("lasso");
        }
        Ok(())
    })
}

fn edit_config(f: impl FnOnce(&mut serde_json::Value) -> Result<(), String>) -> Result<String, String> {
    let path = config_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
    }
    let mut root = match std::fs::read_to_string(&path) {
        Ok(text) => {
            let v: serde_json::Value =
                serde_json::from_str(&text).map_err(|e| format!("parse {}: {e}", path.display()))?;
            v
        }
        Err(_) => serde_json::json!({ "$schema": SCHEMA, "mcpServers": {} }),
    };
    f(&mut root)?;
    let out = serde_json::to_string_pretty(&root).unwrap() + "\n";
    std::fs::write(&path, out).map_err(|e| format!("write {}: {e}", path.display()))?;
    Ok(format!("Wrote lasso MCP server to {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_is_idempotent_and_preserves_other_servers() {
        let dir = std::env::temp_dir().join(format!("lasso-reg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("PI_CODING_AGENT_DIR", &dir);
        // pre-existing config with another server
        std::fs::write(
            dir.join("mcp.json"),
            r#"{"mcpServers": {"other": {"command": "foo"}}}"#,
        )
        .unwrap();

        register().unwrap();
        let text = std::fs::read_to_string(dir.join("mcp.json")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(
            v["mcpServers"]["lasso"]["url"],
            crate::mcp::ENDPOINT
        );
        assert_eq!(v["mcpServers"]["other"]["command"], "foo");

        register().unwrap();
        let v2: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("mcp.json")).unwrap()).unwrap();
        assert_eq!(v2, v, "second register must not duplicate");

        unregister().unwrap();
        let v3: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("mcp.json")).unwrap()).unwrap();
        assert!(v3["mcpServers"].get("lasso").is_none());
        std::fs::remove_dir_all(&dir).ok();
    }
}
