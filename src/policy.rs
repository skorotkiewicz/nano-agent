//! ACP restriction policy: which tools are exposed and where shell commands
//! may run when this process acts as a restricted ACP child.

use crate::state::{acp_mode, context_cwd, env_flag_is_false};
use nano_agent::paths::path_is_inside;
use std::env;
use std::path::{Path, PathBuf};

pub const NANO_ACP_ALLOWED_ROOT_ENV: &str = "NANO_ACP_ALLOWED_ROOT";
pub const NANO_ACP_TOOLS_ENV: &str = "NANO_ACP_TOOLS";

fn acp_spawn_policy_active() -> bool {
    acp_mode()
        && (env::var_os(NANO_ACP_TOOLS_ENV).is_some()
            || env::var_os(NANO_ACP_ALLOWED_ROOT_ENV).is_some())
}

fn acp_tools_enabled() -> bool {
    if !acp_mode() {
        return true;
    }

    env::var(NANO_ACP_TOOLS_ENV)
        .map(|value| !env_flag_is_false(&value))
        .unwrap_or(true)
}

pub fn expose_execute_shell_tools() -> bool {
    acp_tools_enabled()
}

#[cfg(feature = "acp")]
pub fn expose_acp_delegate_tools() -> bool {
    acp_tools_enabled()
}

pub fn expose_mcp_tools() -> bool {
    !acp_spawn_policy_active()
}

fn configured_acp_root() -> Result<Option<PathBuf>, String> {
    if !acp_mode() {
        return Ok(None);
    }

    let Some(root) = env::var_os(NANO_ACP_ALLOWED_ROOT_ENV) else {
        return Ok(None);
    };
    let root = root.to_string_lossy();
    let root = root.trim();
    if root.is_empty() {
        return Err(format!("{NANO_ACP_ALLOWED_ROOT_ENV} is empty"));
    }

    let root = PathBuf::from(root);
    let root = if root.is_absolute() {
        root
    } else {
        env::current_dir()
            .map_err(|error| format!("failed to resolve ACP working directory: {error}"))?
            .join(root)
    };
    let root =
        std::fs::canonicalize(&root).map_err(|error| format!("'{}': {error}", root.display()))?;
    if !root.is_dir() {
        return Err(format!("'{}' is not a directory", root.display()));
    }
    Ok(Some(root))
}

pub fn acp_allowed_root() -> Option<PathBuf> {
    configured_acp_root().ok().flatten()
}

fn shell_cwd_from_args(args: &serde_json::Value) -> Result<PathBuf, String> {
    let cwd = args.get("cwd").and_then(|c| c.as_str()).unwrap_or(".");
    let base = context_cwd();
    let cwd = if cwd == "." || cwd.is_empty() {
        base
    } else {
        let cwd = PathBuf::from(cwd);
        if cwd.is_absolute() {
            cwd
        } else {
            base.join(cwd)
        }
    };

    let cwd =
        std::fs::canonicalize(&cwd).map_err(|error| format!("cwd '{}': {error}", cwd.display()))?;
    if !cwd.is_dir() {
        return Err(format!("cwd '{}' is not a directory", cwd.display()));
    }
    Ok(cwd)
}

fn validate_acp_shell_access(run_cwd: &Path) -> Result<Option<PathBuf>, String> {
    if !acp_mode() {
        return Ok(None);
    }
    if !acp_tools_enabled() {
        return Err(
            "ACP tools are disabled because acp_agents.working_directory is not configured"
                .to_string(),
        );
    }

    let Some(root) = configured_acp_root()? else {
        return Ok(None);
    };
    if path_is_inside(&root, run_cwd) {
        Ok(Some(root))
    } else {
        Err(format!(
            "cwd '{}' is outside ACP working_directory '{}'",
            run_cwd.display(),
            root.display()
        ))
    }
}

/// Resolve and validate the working directory for a shell command.
/// Returns `(run_cwd, writable_root, force_sandbox)`.
pub fn prepare_shell_execution(
    args: &serde_json::Value,
) -> Result<(PathBuf, PathBuf, bool), String> {
    let run_cwd = shell_cwd_from_args(args).map_err(|error| format!("bad arguments: {error}"))?;
    let restricted_root =
        validate_acp_shell_access(&run_cwd).map_err(|error| format!("denied: {error}"))?;
    let force_sandbox = restricted_root.is_some();
    let writable_root = restricted_root.unwrap_or_else(|| run_cwd.clone());
    Ok((run_cwd, writable_root, force_sandbox))
}
