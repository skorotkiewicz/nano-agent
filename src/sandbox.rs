//! Command sandboxing with bubblewrap (bwrap).

use std::path::PathBuf;

use tokio::process::Command;

/// How aggressively to isolate a shell command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxMode {
    /// No bubblewrap — host shell.
    Off,
    /// Filesystem isolation; no network (`--unshare-all`).
    Fs,
    /// Filesystem isolation plus host network (`--share-net`).
    FsNet,
    /// Host network with only runtime files and private scratch space.
    NetOnly,
}

impl SandboxMode {
    /// Parse `NANO_SANDBOX` (and friends). Empty/unset defaults to `Fs`.
    ///
    /// Accepts: `0|false|no|off`, `1|true|yes|on|fs`,
    /// `net|fs+net|share-net`, and `net-only`.
    pub fn from_env_value(value: Option<&str>) -> Self {
        match value.map(str::trim).filter(|v| !v.is_empty()) {
            None => Self::Fs,
            Some(v) if is_false_flag(v) => Self::Off,
            Some(v) if is_net_only_mode(v) => Self::NetOnly,
            Some(v) if is_net_mode(v) => Self::FsNet,
            Some(_) => Self::Fs,
        }
    }

    /// Parse modes that may be persisted for a trusted project.
    pub fn from_project_value(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "fs" => Some(Self::Fs),
            "fs+net" => Some(Self::FsNet),
            "net-only" => Some(Self::NetOnly),
            _ => None,
        }
    }

    pub fn enabled(self) -> bool {
        !matches!(self, Self::Off)
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Fs => "fs",
            Self::FsNet => "fs+net",
            Self::NetOnly => "net-only",
        }
    }
}

fn is_false_flag(value: &str) -> bool {
    matches!(
        value.to_ascii_lowercase().as_str(),
        "0" | "false" | "no" | "off"
    )
}

fn is_net_mode(value: &str) -> bool {
    matches!(
        value.to_ascii_lowercase().as_str(),
        "net" | "fs+net" | "fs_net" | "share-net" | "sharenet" | "network"
    )
}

fn is_net_only_mode(value: &str) -> bool {
    matches!(
        value.to_ascii_lowercase().as_str(),
        "net-only" | "net_only" | "network-only"
    )
}

#[derive(Debug, Clone)]
pub struct Sandbox {
    mode: SandboxMode,
    cwd: PathBuf,
    shell: String,
    restrict_to_cwd: bool,
}

impl Sandbox {
    pub fn new(enabled: bool) -> Self {
        Self::with_mode(if enabled {
            SandboxMode::Fs
        } else {
            SandboxMode::Off
        })
    }

    pub fn with_mode(mode: SandboxMode) -> Self {
        let cwd = std::env::current_dir().unwrap_or_default();
        Sandbox {
            mode,
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

    pub fn mode(&self) -> SandboxMode {
        self.mode
    }

    pub fn wrap_command(&self, command: &str) -> Command {
        if !self.mode.enabled() {
            let mut cmd = Command::new(&self.shell);
            cmd.arg("-c").arg(command);
            return cmd;
        }

        if self.mode == SandboxMode::NetOnly {
            return self.wrap_net_only(command);
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
        cmd.arg("--unshare-all");
        if matches!(self.mode, SandboxMode::FsNet) {
            cmd.arg("--share-net");
        }
        cmd.args(["--die-with-parent", &self.shell, "-c", command]);
        cmd
    }

    fn wrap_net_only(&self, command: &str) -> Command {
        let mut cmd = Command::new("bwrap");

        // Executables and shared libraries only; do not expose the host root,
        // project cwd, home directories, or persistent writable storage.
        for path in [
            "/usr",
            "/bin",
            "/sbin",
            "/lib",
            "/lib64",
            "/nix/store",
            "/run/current-system",
        ] {
            cmd.arg("--ro-bind-try").arg(path).arg(path);
        }

        // Minimal name-resolution and TLS material needed by network clients.
        cmd.args(["--dir", "/etc"]);
        for path in [
            "/etc/resolv.conf",
            "/etc/hosts",
            "/etc/nsswitch.conf",
            "/etc/gai.conf",
            "/etc/localtime",
            "/etc/ssl/certs",
            "/etc/pki/ca-trust",
            "/etc/pki/tls/certs/ca-bundle.crt",
        ] {
            cmd.arg("--ro-bind-try").arg(path).arg(path);
        }

        cmd.args(["--proc", "/proc", "--dev", "/dev"]);
        cmd.args(["--tmpfs", "/tmp", "--tmpfs", "/work"]);
        cmd.args(["--chdir", "/work", "--unshare-all", "--share-net"]);
        cmd.args(["--setenv", "HOME", "/tmp", "--setenv", "TMPDIR", "/tmp"]);
        cmd.args(["--die-with-parent", &self.shell, "-c", command]);
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

/// Recreate any parent directories of `bind_path` that live under a hidden root,
/// so the bind mount target exists inside the sandbox.
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
        assert_eq!(sb.mode(), SandboxMode::Off);
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

    #[test]
    fn sandbox_mode_from_env_value() {
        assert_eq!(SandboxMode::from_env_value(None), SandboxMode::Fs);
        assert_eq!(SandboxMode::from_env_value(Some("")), SandboxMode::Fs);
        assert_eq!(SandboxMode::from_env_value(Some("0")), SandboxMode::Off);
        assert_eq!(SandboxMode::from_env_value(Some("off")), SandboxMode::Off);
        assert_eq!(SandboxMode::from_env_value(Some("fs")), SandboxMode::Fs);
        assert_eq!(SandboxMode::from_env_value(Some("1")), SandboxMode::Fs);
        assert_eq!(SandboxMode::from_env_value(Some("net")), SandboxMode::FsNet);
        assert_eq!(
            SandboxMode::from_env_value(Some("fs+net")),
            SandboxMode::FsNet
        );
        assert_eq!(
            SandboxMode::from_env_value(Some("net-only")),
            SandboxMode::NetOnly
        );
        assert_eq!(
            SandboxMode::from_project_value("net-only"),
            Some(SandboxMode::NetOnly)
        );
        assert_eq!(SandboxMode::from_project_value("off"), None);
    }

    #[test]
    fn fs_net_adds_share_net() {
        let sb = Sandbox::with_mode(SandboxMode::FsNet).with_cwd(PathBuf::from("/tmp"));
        let cmd = sb.wrap_command("true");
        let debug = format!("{:?}", cmd.as_std());
        assert!(debug.contains("share-net") || debug.contains("\"--share-net\""));
        assert!(debug.contains("unshare-all") || debug.contains("\"--unshare-all\""));
    }

    #[test]
    fn net_only_hides_host_files_and_uses_private_scratch() {
        let sb =
            Sandbox::with_mode(SandboxMode::NetOnly).with_cwd(PathBuf::from("/secret/project"));
        let cmd = sb.wrap_command("true");
        let debug = format!("{:?}", cmd.as_std());

        assert!(debug.contains("share-net"));
        assert!(debug.contains("/work"));
        assert!(debug.contains("/tmp"));
        assert!(debug.contains("/usr"));
        assert!(!debug.contains("/secret/project"));
        assert!(!debug.contains("\"/\" \"/\""));
    }
}
