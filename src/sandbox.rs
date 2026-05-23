use std::path::PathBuf;

use tokio::process::Command;

#[derive(Debug, Clone)]
pub struct Sandbox {
    enabled: bool,
    cwd: PathBuf,
    shell: String,
}

impl Sandbox {
    pub fn new(enabled: bool) -> Self {
        let cwd = std::env::current_dir().unwrap_or_default();
        Sandbox {
            enabled,
            cwd,
            shell: "bash".to_string(),
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

    pub fn wrap_command(&self, command: &str) -> Command {
        if !self.enabled {
            let mut cmd = Command::new(&self.shell);
            cmd.arg("-c").arg(command);
            return cmd;
        }

        let mut cmd = Command::new("bwrap");
        cmd.args(["--ro-bind", "/", "/", "--bind"]);
        cmd.arg(self.cwd.as_os_str());
        cmd.arg(self.cwd.as_os_str());
        cmd.args([
            "--proc",
            "/proc",
            "--dev",
            "/dev",
            "--tmpfs",
            "/tmp",
            "--unshare-all",
            "--die-with-parent",
            &self.shell,
            "-c",
            command,
        ]);
        cmd
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
}
