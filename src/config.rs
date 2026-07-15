//! Configuration: load, merge, and expose provider/MCP/ACP/mito settings.

use crate::sandbox::SandboxMode;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};

const NANO_TRUST_PROJECT_CONFIG_ENV: &str = "NANO_TRUST_PROJECT_CONFIG";

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CustomProvider {
    pub provider_type: String,
    pub base_url: String,
    pub api_key: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
}

fn enabled_by_default() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerConfig {
    #[serde(default = "enabled_by_default")]
    pub enabled: bool,
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub headers: HashMap<String, String>,
    #[serde(default)]
    pub show_logs: bool,
}

impl Default for McpServerConfig {
    fn default() -> Self {
        Self {
            enabled: enabled_by_default(),
            command: None,
            args: Vec::new(),
            env: HashMap::new(),
            url: None,
            headers: HashMap::new(),
            show_logs: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MitoModeConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
}

fn default_acp_timeout_secs() -> u64 {
    600
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AcpAgentConfig {
    #[serde(default = "enabled_by_default")]
    pub enabled: bool,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    #[serde(default)]
    pub working_directory: Option<String>,
    #[serde(default = "default_acp_timeout_secs")]
    pub timeout_secs: u64,
}

impl Default for AcpAgentConfig {
    fn default() -> Self {
        Self {
            enabled: enabled_by_default(),
            command: String::new(),
            args: vec![],
            env: HashMap::new(),
            working_directory: None,
            timeout_secs: default_acp_timeout_secs(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Config {
    pub model: Option<String>,
    pub provider: Option<String>,
    pub max_tokens: Option<u32>,
    pub temperature: Option<f32>,
    #[serde(default)]
    pub custom_providers: HashMap<String, CustomProvider>,
    #[serde(default)]
    pub mcp_servers: HashMap<String, McpServerConfig>,
    #[serde(default)]
    pub acp_agents: HashMap<String, AcpAgentConfig>,
    #[serde(default, rename = "mito-mode", alias = "mito_mode")]
    pub mito_mode: MitoModeConfig,
    /// Runtime-only sandbox preference read from the trusted-project marker.
    #[serde(skip)]
    pub project_sandbox: Option<SandboxMode>,
}

impl Config {
    pub fn load() -> Self {
        Self::try_load().unwrap_or_default()
    }

    pub fn try_load() -> Result<Self, String> {
        let global = Self::load_from_path(&config_path_global())?;
        let local_path = config_path_local();
        let trust = if local_path.exists() {
            project_config_trust(&local_path)?
        } else {
            ProjectConfigTrust::denied()
        };
        let local = trust
            .allowed
            .then(|| Self::load_from_path(&local_path))
            .transpose()?
            .flatten();

        let mut config = match (global, local) {
            (Some(mut g), Some(l)) => {
                merge_config_value(&mut g, l);
                serde_json::from_value(g).map_err(|error| format!("invalid merged config: {error}"))
            }
            (Some(g), None) | (None, Some(g)) => {
                serde_json::from_value(g).map_err(|error| format!("invalid config: {error}"))
            }
            (None, None) => Ok(Self::default()),
        }?;
        config.project_sandbox = trust.sandbox;
        Ok(config)
    }

    fn load_from_path(path: &PathBuf) -> Result<Option<Value>, String> {
        if path.exists() {
            let data = std::fs::read_to_string(path)
                .map_err(|error| format!("failed to read '{}': {error}", path.display()))?;
            let mut value: Value = serde_json::from_str(&data)
                .map_err(|error| format!("invalid JSON in '{}': {error}", path.display()))?;
            normalize_config_value(&mut value);
            Ok(Some(value))
        } else {
            Ok(None)
        }
    }

    pub fn get_model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    pub fn get_provider(&self) -> Option<&str> {
        self.provider.as_deref()
    }

    pub fn get_max_tokens(&self) -> Option<u32> {
        self.max_tokens
    }

    pub fn get_temperature(&self) -> Option<f32> {
        self.temperature
    }

    pub fn get_custom_provider(&self, name: &str) -> Option<&CustomProvider> {
        self.custom_providers.get(name)
    }

    pub fn get_mito_mode(&self) -> &MitoModeConfig {
        &self.mito_mode
    }

    pub fn has_enabled_acp_agents(&self) -> bool {
        self.acp_agents.values().any(|agent| agent.enabled)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ProjectConfigTrust {
    allowed: bool,
    sandbox: Option<SandboxMode>,
}

impl ProjectConfigTrust {
    fn denied() -> Self {
        Self {
            allowed: false,
            sandbox: None,
        }
    }

    fn allowed(sandbox: Option<SandboxMode>) -> Self {
        Self {
            allowed: true,
            sandbox,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct StoredProjectTrust {
    sandbox: SandboxMode,
    legacy: bool,
}

fn project_config_enabled(value: Option<&str>) -> bool {
    value.is_some_and(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

fn project_config_trust(config_path: &Path) -> Result<ProjectConfigTrust, String> {
    let project = config_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .canonicalize()
        .map_err(|error| {
            format!(
                "failed to resolve project path '{}': {error}",
                config_path.display()
            )
        })?;
    let marker = trust_marker_path(&project);
    if let Some(stored) = read_trust_marker(&marker, &project) {
        let sandbox = if stored.legacy && io::stdin().is_terminal() && io::stderr().is_terminal() {
            let sandbox = prompt_project_sandbox()?;
            remember_trusted_project(&marker, &project, sandbox)?;
            eprintln!("updated trusted project with sandbox={}", sandbox.label());
            sandbox
        } else {
            stored.sandbox
        };
        return Ok(ProjectConfigTrust::allowed(Some(sandbox)));
    }

    if project_config_enabled(std::env::var(NANO_TRUST_PROJECT_CONFIG_ENV).ok().as_deref()) {
        return Ok(ProjectConfigTrust::allowed(None));
    }

    if !(io::stdin().is_terminal() && io::stderr().is_terminal()) {
        eprintln!(
            "ignoring '{}'; run Nano interactively once to trust this path, or set {NANO_TRUST_PROJECT_CONFIG_ENV}=1",
            config_path.display()
        );
        return Ok(ProjectConfigTrust::denied());
    }

    eprintln!(
        "project config may redirect API requests or start MCP commands:\n  {}",
        config_path.display()
    );
    eprint!(
        "trust project path '{}' and remember? [y/N] ",
        project.display()
    );
    io::stderr()
        .flush()
        .map_err(|error| format!("failed to show project trust prompt: {error}"))?;

    let mut answer = String::new();
    io::stdin()
        .read_line(&mut answer)
        .map_err(|error| format!("failed to read project trust response: {error}"))?;
    if !trust_answer(&answer) {
        eprintln!("project config ignored");
        return Ok(ProjectConfigTrust::denied());
    }

    let sandbox = prompt_project_sandbox()?;
    remember_trusted_project(&marker, &project, sandbox)?;
    eprintln!("project path trusted with sandbox={}", sandbox.label());
    Ok(ProjectConfigTrust::allowed(Some(sandbox)))
}

fn trust_marker_path(project: &Path) -> PathBuf {
    use crate::paths::{cwd_session_key, nano_trusted_projects_dir};

    nano_trusted_projects_dir().join(cwd_session_key(&project.to_string_lossy()))
}

fn read_trust_marker(marker: &Path, project: &Path) -> Option<StoredProjectTrust> {
    let stored = std::fs::read(marker).ok()?;
    let project_bytes = project.as_os_str().as_encoded_bytes();
    if stored == project_bytes {
        return Some(StoredProjectTrust {
            sandbox: SandboxMode::Fs,
            legacy: true,
        });
    }

    let newline = stored.iter().position(|byte| *byte == b'\n')?;
    let header = std::str::from_utf8(&stored[..newline]).ok()?;
    let sandbox = SandboxMode::from_project_value(header.strip_prefix("sandbox=")?)?;
    (&stored[newline + 1..] == project_bytes).then_some(StoredProjectTrust {
        sandbox,
        legacy: false,
    })
}

fn remember_trusted_project(
    marker: &Path,
    project: &Path,
    sandbox: SandboxMode,
) -> Result<(), String> {
    crate::paths::ensure_nano_dirs();
    let contents = trust_marker_contents(project, sandbox);
    std::fs::write(marker, contents)
        .map_err(|error| format!("failed to remember trusted project: {error}"))?;
    crate::paths::ensure_nano_dirs();
    Ok(())
}

fn trust_marker_contents(project: &Path, sandbox: SandboxMode) -> Vec<u8> {
    let mut contents = format!("sandbox={}\n", sandbox.label()).into_bytes();
    contents.extend_from_slice(project.as_os_str().as_encoded_bytes());
    contents
}

fn trust_answer(answer: &str) -> bool {
    matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

fn prompt_project_sandbox() -> Result<SandboxMode, String> {
    eprintln!("shell sandbox for this project:");
    eprintln!("  fs        project files, no network (default)");
    eprintln!("  fs+net    project files and network");
    eprintln!("  net-only  network with no project/home files");
    eprint!("sandbox [fs/fs+net/net-only]: ");
    io::stderr()
        .flush()
        .map_err(|error| format!("failed to show sandbox prompt: {error}"))?;

    let mut answer = String::new();
    io::stdin()
        .read_line(&mut answer)
        .map_err(|error| format!("failed to read sandbox response: {error}"))?;
    Ok(project_sandbox_answer(&answer))
}

fn project_sandbox_answer(answer: &str) -> SandboxMode {
    match answer.trim().to_ascii_lowercase().as_str() {
        "fs+net" | "b" | "both" => SandboxMode::FsNet,
        "net-only" | "n" => SandboxMode::NetOnly,
        _ => SandboxMode::Fs,
    }
}

/// Accept the `mito_mode` alias but store it as the canonical `mito-mode` key.
fn normalize_config_value(value: &mut Value) {
    let Some(object) = value.as_object_mut() else {
        return;
    };
    if !object.contains_key("mito-mode")
        && let Some(alias) = object.remove("mito_mode")
    {
        object.insert("mito-mode".to_string(), alias);
    }
}

/// Deep-merge `overlay` onto `base`, treating overlapping objects as merges.
fn merge_config_value(base: &mut Value, overlay: Value) {
    match (base, overlay) {
        (Value::Object(base_map), Value::Object(overlay_map)) => {
            for (key, value) in overlay_map {
                if let Some(existing) = base_map.get_mut(&key) {
                    merge_config_value(existing, value);
                } else {
                    base_map.insert(key, value);
                }
            }
        }
        (base_slot, overlay_value) => *base_slot = overlay_value,
    }
}

fn config_path_global() -> PathBuf {
    use crate::paths::{ensure_nano_dirs, nano_config_path};
    ensure_nano_dirs();
    let path = nano_config_path();
    // One-shot migrate from XDG ~/.config/nano/config.json if ~/.nano/config.json is missing.
    if !path.exists() {
        let legacy = dirs::config_dir()
            .unwrap_or_default()
            .join("nano")
            .join("config.json");
        if legacy.exists() {
            let _ = std::fs::copy(&legacy, &path);
            ensure_nano_dirs();
        }
    }
    path
}

fn config_path_local() -> PathBuf {
    // Project-local still wins as overlay (not under ~/.nano — lives next to the work).
    std::env::current_dir()
        .unwrap_or_default()
        .join("nano_config.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_config_requires_explicit_trust() {
        assert!(!project_config_enabled(None));
        assert!(!project_config_enabled(Some("")));
        assert!(!project_config_enabled(Some("maybe")));
        assert!(project_config_enabled(Some("1")));
        assert!(project_config_enabled(Some("yes")));
        assert!(trust_answer("y\n"));
        assert!(trust_answer("YES"));
        assert!(!trust_answer(""));
        assert!(!trust_answer("no"));
        assert_eq!(project_sandbox_answer(""), SandboxMode::Fs);
        assert_eq!(project_sandbox_answer("fs+net"), SandboxMode::FsNet);
        assert_eq!(project_sandbox_answer("n"), SandboxMode::NetOnly);
    }

    #[test]
    fn trust_marker_stores_sandbox_for_only_the_exact_project_path() {
        let root =
            std::env::temp_dir().join(format!("nano-project-trust-test-{}", std::process::id()));
        let project = root.join("project");
        let other = root.join("other");
        let markers = root.join("markers");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        std::fs::create_dir_all(&markers).unwrap();
        let project = project.canonicalize().unwrap();
        let other = other.canonicalize().unwrap();
        let marker = markers.join("trusted");

        // Existing path-only markers remain trusted and default to `fs`.
        std::fs::write(&marker, project.as_os_str().as_encoded_bytes()).unwrap();
        assert_eq!(
            read_trust_marker(&marker, &project),
            Some(StoredProjectTrust {
                sandbox: SandboxMode::Fs,
                legacy: true,
            })
        );

        let contents = trust_marker_contents(&project, SandboxMode::FsNet);
        std::fs::write(&marker, contents).unwrap();

        assert_eq!(
            read_trust_marker(&marker, &project),
            Some(StoredProjectTrust {
                sandbox: SandboxMode::FsNet,
                legacy: false,
            })
        );
        assert_eq!(read_trust_marker(&marker, &other), None);

        let contents = trust_marker_contents(&project, SandboxMode::NetOnly);
        assert!(contents.starts_with(b"sandbox=net-only\n"));
        std::fs::write(&marker, contents).unwrap();
        assert_eq!(
            read_trust_marker(&marker, &project),
            Some(StoredProjectTrust {
                sandbox: SandboxMode::NetOnly,
                legacy: false,
            })
        );

        std::fs::write(&marker, b"sandbox=off\n/path").unwrap();
        assert_eq!(read_trust_marker(&marker, &project), None);

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn test_config_default() {
        let config = Config::default();
        assert!(config.model.is_none());
        assert!(config.provider.is_none());
        assert!(config.max_tokens.is_none());
        assert!(config.temperature.is_none());
        assert!(!config.mito_mode.enabled);
    }

    #[test]
    fn test_config_load_valid_json() {
        let config = r#"{"model": "test-model", "provider": "test-provider"}"#;
        let parsed: Config = serde_json::from_str(config).unwrap();
        assert_eq!(parsed.model, Some("test-model".to_string()));
        assert_eq!(parsed.provider, Some("test-provider".to_string()));
    }

    #[test]
    fn test_load_config_with_providers() {
        let config = r#"{
            "custom_providers": {
                "test": {
                    "provider_type": "openai",
                    "base_url": "http://test.com",
                    "api_key": "key"
                }
            }
        }"#;
        let parsed: Config = serde_json::from_str(config).unwrap();
        assert!(parsed.get_custom_provider("test").is_some());
    }

    #[test]
    fn test_config_load_missing() {
        let config = Config::load();
        assert!(config.model.is_none() || config.model.is_some());
    }

    #[test]
    fn test_getters() {
        let config = Config {
            model: Some("gpt-4".to_string()),
            provider: Some("openai".to_string()),
            max_tokens: Some(4096),
            temperature: Some(0.7),
            ..Default::default()
        };
        assert_eq!(config.get_model(), Some("gpt-4"));
        assert_eq!(config.get_provider(), Some("openai"));
        assert_eq!(config.get_max_tokens(), Some(4096));
        assert_eq!(config.get_temperature(), Some(0.7));
    }

    #[test]
    fn test_load_config_with_mcp_servers() {
        let config = r#"{
            "mcp_servers": {
                "semble": {
                    "command": "uvx",
                    "args": ["--from", "semble[mcp]", "semble"]
                }
            }
        }"#;
        let parsed: Config = serde_json::from_str(config).unwrap();
        assert!(parsed.mcp_servers.contains_key("semble"));
    }

    #[test]
    fn test_mcp_server_config_defaults() {
        let config = r#"{ "mcp_servers": { "test": {} } }"#;
        let parsed: Config = serde_json::from_str(config).unwrap();
        let server = parsed.mcp_servers.get("test").unwrap();
        assert!(server.enabled);
        assert!(server.command.is_none());
        assert!(server.args.is_empty());
        assert!(server.env.is_empty());
        assert!(server.url.is_none());
        assert!(!server.show_logs);
    }

    #[test]
    fn test_mcp_server_config_show_logs() {
        let config = r#"{
            "mcp_servers": { "local": { "command": "uvx", "show_logs": true } }
        }"#;
        let parsed: Config = serde_json::from_str(config).unwrap();
        assert!(parsed.mcp_servers.get("local").unwrap().show_logs);
    }

    #[test]
    fn test_mcp_server_config_url() {
        let config = r#"{
            "mcp_servers": {
                "context7": {
                    "url": "https://mcp.context7.com/mcp",
                    "headers": { "CONTEXT7_API_KEY": "ctx7sk-..." }
                }
            }
        }"#;
        let parsed: Config = serde_json::from_str(config).unwrap();
        let server = parsed.mcp_servers.get("context7").unwrap();
        assert!(server.url.is_some());
        assert_eq!(server.url.as_ref().unwrap(), "https://mcp.context7.com/mcp");
        assert!(server.headers.contains_key("CONTEXT7_API_KEY"));
    }

    #[test]
    fn test_load_config_with_acp_agents() {
        let config = r#"{
            "acp_agents": {
                "coder": {
                    "command": "nano-agent",
                    "args": ["--acp"],
                    "working_directory": "/tmp/project",
                    "timeout_secs": 120
                },
                "paused": {
                    "enabled": false,
                    "command": "nano-agent"
                }
            }
        }"#;
        let parsed: Config = serde_json::from_str(config).unwrap();
        let agent = parsed.acp_agents.get("coder").unwrap();
        assert_eq!(agent.command, "nano-agent");
        assert_eq!(agent.args, vec!["--acp"]);
        assert_eq!(agent.working_directory.as_deref(), Some("/tmp/project"));
        assert_eq!(agent.timeout_secs, 120);
        assert!(agent.enabled);
        assert!(!parsed.acp_agents.get("paused").unwrap().enabled);
        assert!(parsed.has_enabled_acp_agents());
    }

    #[test]
    fn integration_entries_default_to_enabled_and_can_be_disabled() {
        let config = r#"{
            "mcp_servers": {
                "active": {},
                "paused": { "enabled": false }
            },
            "acp_agents": {
                "active": { "command": "nano-agent" },
                "paused": { "enabled": false, "command": "nano-agent" }
            }
        }"#;
        let parsed: Config = serde_json::from_str(config).unwrap();

        assert!(parsed.mcp_servers["active"].enabled);
        assert!(!parsed.mcp_servers["paused"].enabled);
        assert!(parsed.acp_agents["active"].enabled);
        assert!(!parsed.acp_agents["paused"].enabled);
        assert!(McpServerConfig::default().enabled);
        assert!(AcpAgentConfig::default().enabled);
    }

    #[test]
    fn test_load_config_with_mito_mode() {
        let config = r#"{
            "mito-mode": { "enabled": true, "provider": "local-gemma4", "model": "gemma4" }
        }"#;
        let parsed: Config = serde_json::from_str(config).unwrap();
        let mito = parsed.get_mito_mode();
        assert!(mito.enabled);
        assert_eq!(mito.provider.as_deref(), Some("local-gemma4"));
        assert_eq!(mito.model.as_deref(), Some("gemma4"));
    }

    #[test]
    fn test_load_config_with_mito_mode_alias() {
        let config = r#"{ "mito_mode": { "enabled": true, "provider": "local" } }"#;
        let parsed: Config = serde_json::from_str(config).unwrap();
        assert!(parsed.mito_mode.enabled);
        assert_eq!(parsed.mito_mode.provider.as_deref(), Some("local"));
    }

    #[test]
    fn merge_overlays_local_fields_and_maps() {
        let mut global: Value = serde_json::from_str(
            r#"{
                "model": "global-model",
                "provider": "global",
                "max_tokens": 100,
                "custom_providers": {"a": {"provider_type": "openai", "base_url": "x"}}
            }"#,
        )
        .unwrap();
        let local: Value = serde_json::from_str(
            r#"{ "model": "local-model", "mcp_servers": {"semble": {"command": "uvx"}} }"#,
        )
        .unwrap();
        merge_config_value(&mut global, local);
        let merged: Config = serde_json::from_value(global).unwrap();
        assert_eq!(merged.model.as_deref(), Some("local-model"));
        assert_eq!(merged.provider.as_deref(), Some("global"));
        assert_eq!(merged.max_tokens, Some(100));
        assert!(merged.custom_providers.contains_key("a"));
        assert!(merged.mcp_servers.contains_key("semble"));
    }

    #[test]
    fn merge_allows_local_mito_mode_to_disable_global() {
        let mut global: Value =
            serde_json::from_str(r#"{ "mito-mode": { "enabled": true, "provider": "global" } }"#)
                .unwrap();
        let mut local: Value =
            serde_json::from_str(r#"{ "mito_mode": { "enabled": false } }"#).unwrap();
        normalize_config_value(&mut local);
        merge_config_value(&mut global, local);
        let merged: Config = serde_json::from_value(global).unwrap();
        assert!(!merged.mito_mode.enabled);
        assert_eq!(merged.mito_mode.provider.as_deref(), Some("global"));
    }
}
