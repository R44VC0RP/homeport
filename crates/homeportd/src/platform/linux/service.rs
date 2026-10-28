//! `homeportd service`: a systemd user unit. With lingering on
//! (`loginctl enable-linger`), it runs from boot without a login.

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::platform::ServiceOps;

const UNIT: &str = "homeportd.service";
/// Socket buffer size quic-go and quinn recommend for QUIC.
const QUIC_RMEM: u64 = 7_500_000;

pub struct Service;

fn unit_path() -> io::Result<PathBuf> {
    let home = crate::platform::unix::home().ok_or_else(|| io::Error::other("HOME is not set"))?;
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| home.join(".config"));
    Ok(config.join("systemd/user").join(UNIT))
}

/// One `ExecStart=` word, quoted for systemd.
fn quote(s: &str) -> String {
    let escaped = s.replace('\\', "\\\\").replace('"', "\\\"").replace('%', "%%").replace('$', "$$");
    if escaped.is_empty() || escaped.contains(|c: char| c.is_whitespace() || c == '\'' || c == ';') {
        format!("\"{escaped}\"")
    } else {
        escaped
    }
}

fn unit(exe: &Path, args: &[OsString]) -> String {
    let mut cmd = quote(&exe.to_string_lossy());
    for a in args {
        cmd.push(' ');
        cmd.push_str(&quote(&a.to_string_lossy()));
    }
    format!(
        "[Unit]\n\
         Description=Homeport server\n\
         Wants=network-online.target\n\
         After=network-online.target\n\
         # Keep retrying, e.g. while tailscaled is not up yet after boot.\n\
         StartLimitIntervalSec=0\n\
         \n\
         [Service]\n\
         Type=simple\n\
         ExecStart={cmd}\n\
         Environment=RUST_LOG=info\n\
         Restart=always\n\
         RestartSec=5\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n"
    )
}

/// `systemctl --user`. Sessions that didn't go through `pam_systemd` (some
/// ssh setups, `su`) have no `XDG_RUNTIME_DIR`; the user manager's bus is
/// still at `/run/user/<uid>/bus` when it runs (lingering), so point there.
fn systemctl(args: &[&str]) -> io::Result<std::process::Output> {
    let mut c = Command::new("systemctl");
    c.arg("--user").args(args);
    if std::env::var_os("XDG_RUNTIME_DIR").is_none() {
        // SAFETY: no preconditions.
        let dir = PathBuf::from(format!("/run/user/{}", unsafe { libc::getuid() }));
        if dir.join("bus").exists() {
            c.env("XDG_RUNTIME_DIR", &dir);
        }
    }
    c.output()
}

fn check(out: io::Result<std::process::Output>, what: &str) -> io::Result<()> {
    let out = out?;
    if out.status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "systemctl --user {what} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )))
    }
}

fn user() -> String {
    std::env::var("USER").unwrap_or_else(|_| "$USER".into())
}

/// Whether the user's services run without a login session.
fn linger() -> Option<bool> {
    let out = Command::new("loginctl").args(["show-user", &user(), "-p", "Linger", "--value"]).output().ok()?;
    match String::from_utf8_lossy(&out.stdout).trim() {
        "yes" => Some(true),
        "no" => Some(false),
        _ => None,
    }
}

fn notes() -> String {
    let mut s = String::new();
    if linger() != Some(true) {
        s.push_str(&format!(
            "\n  note: lingering is off, so the server stops when you log out and only starts at login.\n\
             \x20       Run `sudo loginctl enable-linger {}` to start it at boot.",
            user()
        ));
    }
    let rmem = std::fs::read_to_string("/proc/sys/net/core/rmem_max").ok().and_then(|v| v.trim().parse::<u64>().ok());
    if rmem.is_some_and(|r| r < QUIC_RMEM) {
        s.push_str(&format!(
            "\n  tip: larger UDP buffers help QUIC throughput:\n\
             \x20      printf 'net.core.rmem_max={QUIC_RMEM}\\nnet.core.wmem_max={QUIC_RMEM}\\n' | sudo tee /etc/sysctl.d/90-homeport.conf && sudo sysctl --system"
        ));
    }
    s
}

impl ServiceOps for Service {
    fn install(exe: &Path, args: &[OsString], print: bool) -> io::Result<String> {
        let path = unit_path()?;
        let text = unit(exe, args);
        if print {
            return Ok(format!("would write {}:\n{text}", path.display()));
        }
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&path, text)?;
        check(systemctl(&["daemon-reload"]), "daemon-reload")?;
        check(systemctl(&["enable", UNIT]), "enable")?;
        // Restart so a reinstall runs the new binary and arguments.
        check(systemctl(&["restart", UNIT]), "restart")?;
        Ok(format!(
            "installed systemd user unit {UNIT}\n  unit: {}\n  logs: journalctl --user -u homeportd -f{}",
            path.display(),
            notes()
        ))
    }

    fn uninstall() -> io::Result<String> {
        let path = unit_path()?;
        let _ = systemctl(&["disable", "--now", UNIT]);
        let removed = match std::fs::remove_file(&path) {
            Ok(()) => true,
            Err(e) if e.kind() == io::ErrorKind::NotFound => false,
            Err(e) => return Err(e),
        };
        let _ = systemctl(&["daemon-reload"]);
        Ok(if removed {
            format!("removed systemd user unit {UNIT} ({})", path.display())
        } else {
            format!("systemd user unit {UNIT} was not installed")
        })
    }

    fn status() -> io::Result<String> {
        let path = unit_path()?;
        let out = systemctl(&[
            "show",
            UNIT,
            "-p",
            "LoadState,UnitFileState,ActiveState,SubState,MainPID,ExecMainStartTimestamp",
        ])?;
        let text = String::from_utf8_lossy(&out.stdout);
        let field = |k: &str| {
            text.lines().find_map(|l| l.strip_prefix(k).and_then(|v| v.strip_prefix('='))).unwrap_or("").to_owned()
        };
        let installed = path.exists();
        let mut s = format!(
            "systemd user unit {UNIT}: {}",
            if installed { format!("installed ({})", field("UnitFileState")) } else { "not installed".into() }
        );
        if installed {
            s.push_str(&format!("\n  state:   {} ({})", field("ActiveState"), field("SubState")));
            let pid = field("MainPID");
            if !pid.is_empty() && pid != "0" {
                s.push_str(&format!("\n  pid:     {pid}"));
            }
            let since = field("ExecMainStartTimestamp");
            if !since.is_empty() {
                s.push_str(&format!("\n  since:   {since}"));
            }
            s.push_str(&format!("\n  unit:    {}", path.display()));
            s.push_str(&format!(
                "\n  linger:  {}",
                match linger() {
                    Some(true) => "yes (starts at boot)",
                    Some(false) => "no (starts at login only)",
                    None => "unknown",
                }
            ));
        }
        Ok(s)
    }
}
