//! `homeportd service`: a LaunchAgent in the user's GUI session, so the
//! server can use the Trash, Quick Look and Files & Folders grants.

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::platform::ServiceOps;

pub const LABEL: &str = "dev.vogel.homeportd";

pub struct Service;

fn home() -> io::Result<PathBuf> {
    crate::platform::unix::home().ok_or_else(|| io::Error::other("HOME is not set"))
}

fn plist_path() -> io::Result<PathBuf> {
    Ok(home()?.join("Library/LaunchAgents").join(format!("{LABEL}.plist")))
}

fn log_path() -> io::Result<PathBuf> {
    Ok(home()?.join("Library/Logs/Homeport/homeportd.log"))
}

fn domain() -> String {
    // SAFETY: no preconditions.
    format!("gui/{}", unsafe { libc::getuid() })
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn plist(exe: &Path, args: &[OsString], log: &Path) -> String {
    let mut prog = format!("    <string>{}</string>\n", xml_escape(&exe.to_string_lossy()));
    for a in args {
        prog.push_str(&format!("    <string>{}</string>\n", xml_escape(&a.to_string_lossy())));
    }
    let log = xml_escape(&log.to_string_lossy());
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{LABEL}</string>
  <key>ProgramArguments</key>
  <array>
{prog}  </array>
  <key>EnvironmentVariables</key>
  <dict>
    <key>RUST_LOG</key>
    <string>info</string>
  </dict>
  <key>RunAtLoad</key>
  <true/>
  <!-- Restarted if it exits, e.g. when Tailscale is not up yet at login. -->
  <key>KeepAlive</key>
  <true/>
  <key>ThrottleInterval</key>
  <integer>10</integer>
  <key>ProcessType</key>
  <string>Interactive</string>
  <key>StandardOutPath</key>
  <string>{log}</string>
  <key>StandardErrorPath</key>
  <string>{log}</string>
</dict>
</plist>
"#
    )
}

fn launchctl(args: &[&str]) -> io::Result<std::process::Output> {
    Command::new("/bin/launchctl").args(args).output()
}

impl ServiceOps for Service {
    fn install(exe: &Path, args: &[OsString], print: bool) -> io::Result<String> {
        let path = plist_path()?;
        let log = log_path()?;
        let text = plist(exe, args, &log);
        if print {
            return Ok(format!("would write {}:\n{text}", path.display()));
        }
        if let Some(dir) = log.parent() {
            std::fs::create_dir_all(dir)?;
        }
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let target = format!("{}/{LABEL}", domain());
        // Replace a loaded copy.
        let _ = launchctl(&["bootout", &target]);
        std::fs::write(&path, text)?;
        let out = launchctl(&["bootstrap", &domain(), &path.to_string_lossy()])?;
        if !out.status.success() {
            return Err(io::Error::other(format!(
                "launchctl bootstrap failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        Ok(format!(
            "installed LaunchAgent {LABEL}\n  plist: {}\n  logs:  {}\n\
             ~/Downloads and other protected folders ask for access on the first run: click Allow.",
            path.display(),
            log.display()
        ))
    }

    fn uninstall() -> io::Result<String> {
        let path = plist_path()?;
        let _ = launchctl(&["bootout", &format!("{}/{LABEL}", domain())]);
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(format!("removed LaunchAgent {LABEL} ({})", path.display())),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(format!("LaunchAgent {LABEL} was not installed")),
            Err(e) => Err(e),
        }
    }

    fn status() -> io::Result<String> {
        let path = plist_path()?;
        let installed = path.exists();
        let out = launchctl(&["print", &format!("{}/{LABEL}", domain())])?;
        let text = String::from_utf8_lossy(&out.stdout);
        let field = |k: &str| {
            text.lines()
                .map(str::trim)
                .find_map(|l| l.strip_prefix(k).map(|v| v.trim_start_matches([' ', '=']).trim().to_owned()))
        };
        let loaded = out.status.success();
        let mut s = format!(
            "LaunchAgent {LABEL}: {}{}",
            if installed { "installed" } else { "not installed" },
            if loaded { ", loaded" } else { ", not loaded" }
        );
        if loaded {
            if let Some(state) = field("state") {
                s.push_str(&format!("\n  state:   {state}"));
            }
            if let Some(pid) = field("pid") {
                s.push_str(&format!("\n  pid:     {pid}"));
            }
            if let Some(prog) = field("program") {
                s.push_str(&format!("\n  program: {prog}"));
            }
        }
        if installed {
            s.push_str(&format!("\n  plist:   {}", path.display()));
        }
        Ok(s)
    }
}
