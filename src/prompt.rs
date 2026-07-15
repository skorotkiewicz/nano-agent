//! System prompt construction and the doc/skill file finder.

use crate::policy::{acp_allowed_root, expose_execute_shell_tools};
use crate::self_harness::load_active_harness;
#[cfg(feature = "acp")]
use crate::state::get_config;
use crate::state::{context_cwd, no_context};
use dirs::home_dir;
use std::collections::HashMap;
use std::env;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

const SKIP_DIRS: &[&str] = &[
    ".git",
    ".venv",
    "__pycache__",
    "node_modules",
    "venv",
    "target",
];

const DOC_NAMES: &[&str] = &[
    "claude.md",
    "agent.md",
    "agents.md",
    "AGENTS.md",
    "readme.md",
];

// Cached per cwd: the prompt embeds the working directory and the docs found
// under it, and ACP sessions can each have a different cwd.
static SYSTEM_CACHE: OnceLock<Mutex<HashMap<PathBuf, String>>> = OnceLock::new();

pub fn doc_names() -> Vec<&'static str> {
    DOC_NAMES.to_vec()
}

pub fn find_files(roots: Vec<String>, names: Vec<&str>, limit: usize) -> String {
    let home = home_dir().unwrap_or_default();
    let mut found = Vec::new();

    for root in roots {
        let root_path = if root == "~" {
            home.clone()
        } else if let Some(stripped) = root.strip_prefix("~/") {
            home.join(stripped)
        } else if let Some(stripped) = root.strip_prefix('~') {
            home.join(stripped)
        } else {
            PathBuf::from(&root)
        };

        if !root_path.is_dir() {
            continue;
        }

        // ponytail: cap depth — full-tree scans kill startup on monorepos
        for entry in walkdir::WalkDir::new(&root_path)
            .max_depth(5)
            .into_iter()
            .filter_entry(|e| {
                e.file_name()
                    .to_str()
                    .map(|s| !SKIP_DIRS.contains(&s))
                    .unwrap_or(true)
            })
        {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => continue,
            };

            if !entry.file_type().is_file() {
                continue;
            }

            let file_name = entry.file_name().to_str().unwrap_or("").to_lowercase();
            if names.iter().any(|n| file_name == *n) {
                let path = entry.path().to_path_buf();
                let path_str = if path.starts_with(&home) {
                    format!(
                        "~/{}",
                        path.strip_prefix(&home).unwrap().to_str().unwrap_or("")
                    )
                } else {
                    path.to_str().unwrap_or("").to_string()
                };
                found.push(path_str);
                if found.len() >= limit {
                    found.sort();
                    found.dedup();
                    return found.join(", ");
                }
            }
        }
    }

    found.sort();
    found.dedup();
    if found.is_empty() {
        "none".to_string()
    } else {
        found.join(", ")
    }
}

pub fn get_system() -> String {
    if no_context() {
        return String::new();
    }
    let cwd = context_cwd();
    let mut cache = SYSTEM_CACHE
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    cache
        .entry(cwd)
        .or_insert_with_key(|cwd| build_system(cwd))
        .clone()
}

pub fn clear_system_cache() {
    if let Some(cache) = SYSTEM_CACHE.get() {
        cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
    }
}

fn build_system(cwd: &Path) -> String {
    let cwd_path = cwd;
    let cwd = cwd_path.to_str().unwrap_or(".").to_string();
    let home = home_dir()
        .unwrap_or_default()
        .to_str()
        .unwrap_or("")
        .to_string();

    let docs = find_files(vec![cwd.clone()], doc_names(), 12);
    let skills = find_files(
        vec![
            ".claude/skills".to_string(),
            format!("{}/.pi/agent/skills", home),
        ],
        vec!["skill.md", "skills.md"],
        12,
    );
    #[cfg(feature = "acp")]
    let delegation: &str = if !get_config().acp_agents.is_empty() {
        "\n- delegate_task / delegate_tasks: spawn configured ACP child agents for independent subtasks."
    } else {
        ""
    };
    #[cfg(not(feature = "acp"))]
    let delegation: &str = "";
    let acp_restriction = acp_allowed_root()
        .map(|root| format!("\nShell cwd must stay under {}.", root.display()))
        .unwrap_or_default();
    let harness = load_active_harness(cwd_path)
        .map(|harness| format!("\n\n# Local harness\n{harness}"))
        .unwrap_or_default();

    if !expose_execute_shell_tools() {
        return format!(
            "You are Nano in a restricted ACP session. Shell and MCP tools are off.\n\
             Answer from the prompt and context only. Be brief.\n\
             cwd: {cwd}\nplatform: {}{acp_restriction}{harness}",
            env::consts::OS,
        );
    }

    format!(
        "You are Nano, a shell agent in a real terminal.\n\
         \n\
         # How you work\n\
         - Primary tool: execute_shell. For any inspect/edit/run/test/search, call it — do not invent results.\n\
         - description: 5–10 words of why, not a second command.\n\
         - Prefer small read steps first, then change only what the user asked for.\n\
         - Before editing/deleting in a git repo, inspect git status and preserve user changes.\n\
         - Never destroy data (rm -rf, reset --hard, push --force, drop tables) unless the user explicitly asked.\n\
         - Keep going until the task is done or blocked by deny/error; then stop and say what is left.\n\
         - Answer in short plain terminal text. No markdown chrome, no songs, no filler.{delegation}{acp_restriction}\n\
         \n\
         cwd: {cwd}\n\
         platform: {}\n\
         shell: {}\n\
         docs (read if useful): {docs}\n\
         skills (read if useful): {skills}{harness}",
        env::consts::OS,
        env::var("SHELL").unwrap_or_default(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn build_system_includes_active_harness_overlay() {
        let dir = env::temp_dir().join(format!("nano-agent-harness-test-{}", std::process::id()));
        let nano_dir = dir.join(".nano");
        fs::create_dir_all(&nano_dir).unwrap();
        fs::write(
            nano_dir.join("harness.md"),
            "Verify required files before final answer.",
        )
        .unwrap();

        let system = build_system(&dir);
        assert!(system.contains("Local harness"));
        assert!(system.contains("Verify required files before final answer."));
        assert!(system.contains("inspect git status and preserve user changes"));
        assert!(system.contains("You are Nano"));

        let _ = fs::remove_dir_all(dir);
    }
}
