use std::path::PathBuf;

use tokio::process::Command;

#[derive(Debug, Clone)]
pub struct Sandbox {
    enabled: bool,
    cwd: PathBuf,
    shell: String,
    restrict_to_cwd: bool,
}

impl Sandbox {
    pub fn new(enabled: bool) -> Self {
        let cwd = std::env::current_dir().unwrap_or_default();
        Sandbox {
            enabled,
            cwd,
            shell: "bash".to_string(),
            restrict_to_cwd: false,
        }
    }

    pub fn with_shell(mut self, shell: &str) -> Self {
        if !shell.is_empty() {
            self.shell = shell.to_string();
        }
        self
    }

    pub fn with_cwd(mut self, cwd: PathBuf) -> Self {
        self.cwd = cwd;
        self
    }

    pub fn restrict_to_cwd(mut self, restrict: bool) -> Self {
        self.restrict_to_cwd = restrict;
        self
    }

    pub fn wrap_command(&self, command: &str) -> Command {
        if !self.enabled {
            let mut cmd = Command::new(&self.shell);
            cmd.arg("-c").arg(command);
            return cmd;
        }

        let mut cmd = Command::new("bwrap");
        cmd.args(["--ro-bind", "/", "/"]);

        let hidden_roots = hidden_roots(self.restrict_to_cwd);
        for root in &hidden_roots {
            cmd.arg("--tmpfs").arg(root.as_os_str());
        }
        bind_mountpoint_dirs(&mut cmd, &self.cwd, &hidden_roots);

        cmd.arg("--bind")
            .arg(self.cwd.as_os_str())
            .arg(self.cwd.as_os_str());
        cmd.args(["--proc", "/proc", "--dev", "/dev"]);
        cmd.args([
            "--unshare-all",
            "--die-with-parent",
            &self.shell,
            "-c",
            command,
        ]);
        cmd
    }
}

fn hidden_roots(restrict_to_cwd: bool) -> Vec<PathBuf> {
    let mut roots = vec![PathBuf::from("/tmp")];
    if restrict_to_cwd {
        roots.extend(
            ["/home", "/root", "/mnt", "/media", "/run/user"]
                .into_iter()
                .map(PathBuf::from)
                .filter(|path| path.exists()),
        );
    }
    roots
}

fn bind_mountpoint_dirs(cmd: &mut Command, bind_path: &std::path::Path, hidden_roots: &[PathBuf]) {
    let Some(parent) = bind_path.parent() else {
        return;
    };

    for hidden_root in hidden_roots {
        if !bind_path.starts_with(hidden_root) {
            continue;
        }

        let Ok(relative) = parent.strip_prefix(hidden_root) else {
            continue;
        };
        let mut current = hidden_root.clone();
        for component in relative.components() {
            current.push(component.as_os_str());
            cmd.arg("--dir").arg(current.as_os_str());
        }
        if bind_path != hidden_root {
            cmd.arg("--dir").arg(bind_path.as_os_str());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sandbox_new_creates_with_defaults() {
        let sb = Sandbox::new(false);
        assert!(!sb.enabled);
        assert_eq!(sb.shell, "bash");
    }

    #[test]
    fn test_sandbox_with_shell_sets_custom() {
        let sb = Sandbox::new(false).with_shell("sh");
        assert_eq!(sb.shell, "sh");
    }

    #[test]
    fn test_sandbox_with_empty_shell_keeps_default() {
        let sb = Sandbox::new(false).with_shell("");
        // Empty string should keep the original shell (bash)
        assert_eq!(sb.shell, "bash");
    }

    #[test]
    fn test_sandbox_with_cwd() {
        let custom_cwd = PathBuf::from("/tmp");
        let sb = Sandbox::new(true).with_cwd(custom_cwd.clone());
        assert_eq!(sb.cwd, custom_cwd);
    }

    #[test]
    fn test_sandbox_restrict_to_cwd() {
        let sb = Sandbox::new(true).restrict_to_cwd(true);
        assert!(sb.restrict_to_cwd);
    }
}
