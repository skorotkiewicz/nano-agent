//! System prompt construction and the doc/skill file finder.

use crate::policy::{acp_allowed_root, expose_execute_shell_tools};
#[cfg(feature = "acp")]
use crate::state::get_config;
use crate::state::context_cwd;
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

const DOC_NAMES: &[&str] = &["claude.md", "agent.md", "agents.md", "AGENTS.md", "readme.md"];

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
        let root_path = if root.starts_with('~') {
            home.join(&root[2..])
        } else {
            PathBuf::from(&root)
        };

        if !root_path.is_dir() {
            continue;
        }

        for entry in walkdir::WalkDir::new(&root_path)
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

fn build_system(cwd: &Path) -> String {
    let cwd = cwd.to_str().unwrap_or(".").to_string();
    let home = home_dir()
        .unwrap_or_default()
        .to_str()
        .unwrap_or("")
        .to_string();

    let docs = find_files(vec![cwd.clone()], doc_names(), 40);
    let skills = find_files(
        vec![
            ".claude/skills".to_string(),
            format!("{}/.pi/agent/skills", home),
        ],
        vec!["skill.md", "skills.md"],
        40,
    );
    #[cfg(feature = "acp")]
    let delegation = if !get_config().acp_agents.is_empty() {
        " Use delegate_task or delegate_tasks to spawn configured ACP child agents for independent subtasks."
    } else {
        ""
    };
    #[cfg(not(feature = "acp"))]
    let delegation = "";
    let tool_guidance = if expose_execute_shell_tools() {
        "You are Nano, a general-purpose shell agent with a primary tool: execute_shell.\n\
         When user asks for shell commands, ALWAYS make a tool_call to execute_shell\n\
         Use it to inspect, edit, install, test, search, automate, and answer."
    } else {
        "You are Nano, a general-purpose shell agent. Local shell and MCP tools are disabled in this restricted ACP session.\n\
         Answer from the prompt and provided context only."
    };
    let acp_restriction = acp_allowed_root()
        .map(|root| format!(" Local shell commands must stay under {}.", root.display()))
        .unwrap_or_default();
    let persistence = if expose_execute_shell_tools() {
        "Keep taking shell steps until done or blocked."
    } else {
        "Complete the task without tool calls."
    };

    format!(
        "{}\n\
         {}{}\n\
         Be concise, tenacious, and relentlessly useful. {}\n\
         Output short plain-text snippets optimized for terminal reading; no markdown rendering or syntax highlighting.\n\
         Never run destructive commands unless explicitly requested.\n\
         cwd: {}\n\
         platform: {}\n\
         shell: {}\n\
         Important docs (read as needed): {}\n\
         Important skill files (read as needed): {}",
        tool_guidance,
        delegation,
        acp_restriction,
        persistence,
        cwd,
        env::consts::OS,
        env::var("SHELL").unwrap_or_default(),
        docs,
        skills
    )
}
