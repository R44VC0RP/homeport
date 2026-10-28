//! `homeportd service` on Windows: a scheduled task that runs as the user at
//! boot whether or not they are logged on (S4U: no stored password), restarted
//! if it stops, plus an inbound firewall rule for its UDP port from tailnet
//! addresses (that part needs an elevated shell; see
//! `scripts/install-windows.ps1`).
//!
//! S4U tasks run without network credentials (no SMB shares or EFS files),
//! which local shares don't need. Output goes to
//! `%LOCALAPPDATA%\Homeport\logs\homeportd.log`.

use std::ffi::OsString;
use std::io;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::platform::ServiceOps;

pub const TASK: &str = "Homeport server";
pub const FIREWALL_RULE: &str = "Homeport server (homeportd, UDP)";
const NO_WINDOW: u32 = 0x0800_0000;

pub struct Service;

fn log_path() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("USERPROFILE").map(|h| PathBuf::from(h).join("AppData\\Local")))
        .unwrap_or_else(|| PathBuf::from("C:\\"))
        .join("Homeport\\logs\\homeportd.log")
}

/// The user's name for messages.
fn user() -> String {
    let name = std::env::var("USERNAME").unwrap_or_default();
    match std::env::var("USERDOMAIN") {
        Ok(d) if !d.is_empty() => format!("{d}\\{name}"),
        _ => name,
    }
}

/// This process's user as a SID string (`S-1-5-21-…`): exact, unlike
/// `USERDOMAIN\USERNAME`, which is wrong in SSH sessions.
fn user_sid() -> io::Result<String> {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, LocalFree};
    use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
    use windows_sys::Win32::Security::{GetTokenInformation, TOKEN_QUERY, TOKEN_USER, TokenUser};
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    let mut token: HANDLE = std::ptr::null_mut();
    // SAFETY: the current process pseudo-handle; `token` is written on success.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut buf = vec![0u64; 64];
    let mut len = 0u32;
    // SAFETY: `buf` is writable for its byte length.
    let ok =
        unsafe { GetTokenInformation(token, TokenUser, buf.as_mut_ptr().cast(), (buf.len() * 8) as u32, &mut len) };
    // SAFETY: our token handle.
    unsafe { CloseHandle(token) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: filled by GetTokenInformation.
    let sid = unsafe { (*buf.as_ptr().cast::<TOKEN_USER>()).User.Sid };
    let mut s: *mut u16 = std::ptr::null_mut();
    // SAFETY: `sid` points into `buf`; `s` receives a LocalAlloc'd string.
    if unsafe { ConvertSidToStringSidW(sid, &mut s) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a NUL-terminated string from ConvertSidToStringSidW.
    let out = unsafe {
        let n = (0..).take_while(|&i| *s.add(i) != 0).count();
        String::from_utf16_lossy(std::slice::from_raw_parts(s, n))
    };
    // SAFETY: allocated by ConvertSidToStringSidW.
    unsafe { LocalFree(s.cast()) };
    Ok(out)
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// One argument for a command line (CommandLineToArgvW rules).
fn quote(a: &str) -> String {
    if !a.is_empty() && !a.contains([' ', '\t', '"']) {
        return a.to_owned();
    }
    let mut out = String::from("\"");
    let mut backslashes = 0;
    for c in a.chars() {
        match c {
            '\\' => backslashes += 1,
            '"' => {
                out.push_str(&"\\".repeat(backslashes * 2 + 1));
                out.push('"');
                backslashes = 0;
            }
            _ => {
                out.push_str(&"\\".repeat(backslashes));
                out.push(c);
                backslashes = 0;
            }
        }
    }
    out.push_str(&"\\".repeat(backslashes * 2));
    out.push('"');
    out
}

fn port(args: &[OsString]) -> u16 {
    let args: Vec<String> = args.iter().map(|a| a.to_string_lossy().into_owned()).collect();
    for (i, a) in args.iter().enumerate() {
        if let Some(v) = a.strip_prefix("--port=") {
            return v.parse().unwrap_or(homeport_proto::DEFAULT_PORT);
        }
        if a == "--port"
            && let Some(v) = args.get(i + 1)
        {
            return v.parse().unwrap_or(homeport_proto::DEFAULT_PORT);
        }
    }
    homeport_proto::DEFAULT_PORT
}

/// `cmd.exe` arguments running homeportd with its output appended to the log.
fn cmd_args(exe: &Path, args: &[OsString], log: &Path) -> String {
    let mut line = quote(&exe.to_string_lossy());
    for a in args {
        line.push(' ');
        line.push_str(&quote(&a.to_string_lossy()));
    }
    // No color codes in the log file; info level.
    format!("/d /s /c \"set NO_COLOR=1&& set RUST_LOG=info&& {line} >> \"{}\" 2>&1\"", log.display())
}

fn task_xml(exe: &Path, args: &[OsString], log: &Path) -> String {
    let user = xml_escape(&user_sid().unwrap_or_else(|_| user()));
    let cmd = std::env::var("ComSpec").unwrap_or_else(|_| "C:\\Windows\\System32\\cmd.exe".into());
    let arguments = xml_escape(&cmd_args(exe, args, log));
    let dir = xml_escape(&exe.parent().map(|p| p.display().to_string()).unwrap_or_default());
    format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.4" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Description>Homeport server: serves shared folders to Homeport clients over the tailnet.</Description>
    <URI>\{TASK}</URI>
  </RegistrationInfo>
  <Triggers>
    <BootTrigger>
      <Enabled>true</Enabled>
      <!-- Started again if it is not running (e.g. Tailscale was not up yet). -->
      <Repetition>
        <Interval>PT5M</Interval>
        <StopAtDurationEnd>false</StopAtDurationEnd>
      </Repetition>
    </BootTrigger>
  </Triggers>
  <Principals>
    <Principal id="Author">
      <UserId>{user}</UserId>
      <LogonType>S4U</LogonType>
      <RunLevel>LeastPrivilege</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <AllowHardTerminate>true</AllowHardTerminate>
    <StartWhenAvailable>true</StartWhenAvailable>
    <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>
    <IdleSettings>
      <StopOnIdleEnd>false</StopOnIdleEnd>
      <RestartOnIdle>false</RestartOnIdle>
    </IdleSettings>
    <AllowStartOnDemand>true</AllowStartOnDemand>
    <Enabled>true</Enabled>
    <Hidden>false</Hidden>
    <RunOnlyIfIdle>false</RunOnlyIfIdle>
    <WakeToRun>false</WakeToRun>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <!-- The default (7) runs below normal CPU and I/O priority. -->
    <Priority>5</Priority>
    <RestartOnFailure>
      <Interval>PT1M</Interval>
      <Count>999</Count>
    </RestartOnFailure>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>{cmd}</Command>
      <Arguments>{arguments}</Arguments>
      <WorkingDirectory>{dir}</WorkingDirectory>
    </Exec>
  </Actions>
</Task>
"#,
        cmd = xml_escape(&cmd),
    )
}

fn firewall_args(exe: &Path, port: u16) -> Vec<String> {
    vec![
        "advfirewall".into(),
        "firewall".into(),
        "add".into(),
        "rule".into(),
        format!("name={FIREWALL_RULE}"),
        "dir=in".into(),
        "action=allow".into(),
        "protocol=UDP".into(),
        format!("localport={port}"),
        format!("program={}", exe.display()),
        "remoteip=100.64.0.0/10,fd7a:115c:a1e0::/48".into(),
        "profile=any".into(),
    ]
}

fn run(prog: &str, args: &[String]) -> io::Result<std::process::Output> {
    Command::new(prog).args(args).creation_flags(NO_WINDOW).output()
}

fn text(o: &std::process::Output) -> String {
    let mut s = String::from_utf8_lossy(&o.stdout).trim().to_owned();
    let e = String::from_utf8_lossy(&o.stderr);
    if !e.trim().is_empty() {
        if !s.is_empty() {
            s.push('\n');
        }
        s.push_str(e.trim());
    }
    s
}

fn schtasks(args: &[&str]) -> io::Result<std::process::Output> {
    run("schtasks.exe", &args.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>())
}

/// Stops other homeportd processes (not this one: it may be the installer).
fn kill_others() {
    let me = format!("PID ne {}", std::process::id());
    let _ = run("taskkill.exe", &["/F".into(), "/IM".into(), "homeportd.exe".into(), "/FI".into(), me]);
}

fn process_running() -> bool {
    let me = format!("PID ne {}", std::process::id());
    run("tasklist.exe", &["/FI".into(), "IMAGENAME eq homeportd.exe".into(), "/FI".into(), me, "/NH".into()])
        .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).contains("homeportd.exe"))
}

impl ServiceOps for Service {
    fn install(exe: &Path, args: &[OsString], print: bool) -> io::Result<String> {
        let log = log_path();
        let xml = task_xml(exe, args, &log);
        let port = port(args);
        let fw = firewall_args(exe, port);
        let fw_cmd = format!(
            "netsh {}",
            fw.iter()
                .map(|a| if a.contains(' ') { format!("\"{a}\"") } else { a.clone() })
                .collect::<Vec<_>>()
                .join(" ")
        );
        if print {
            return Ok(format!("would register scheduled task \"{TASK}\":\n{xml}\nand run (elevated):\n  {fw_cmd}"));
        }
        if let Some(dir) = log.parent() {
            std::fs::create_dir_all(dir)?;
        }
        // Task Scheduler reads the XML as UTF-16 with a BOM.
        let file = std::env::temp_dir().join(format!("homeportd-task-{}.xml", std::process::id()));
        let bytes: Vec<u8> = std::iter::once(0xFEFFu16).chain(xml.encode_utf16()).flat_map(u16::to_le_bytes).collect();
        std::fs::write(&file, bytes)?;
        let _ = schtasks(&["/End", "/TN", TASK]);
        let created = schtasks(&["/Create", "/TN", TASK, "/XML", &file.to_string_lossy(), "/F"]);
        let _ = std::fs::remove_file(&file);
        let created = created?;
        if !created.status.success() {
            return Err(io::Error::other(format!(
                "schtasks /Create failed (S4U tasks need an administrator shell): {}",
                text(&created)
            )));
        }
        // An older copy may still be running outside the task.
        kill_others();
        let started = schtasks(&["/Run", "/TN", TASK])?;
        let mut report = format!(
            "registered scheduled task \"{TASK}\" (at boot, as {}, whether logged on or not){}\n  logs: {}",
            user(),
            if started.status.success() { ", started" } else { ", not started" },
            log.display()
        );
        let _ = run(
            "netsh.exe",
            &["advfirewall".into(), "firewall".into(), "delete".into(), "rule".into(), format!("name={FIREWALL_RULE}")],
        );
        match run("netsh.exe", &fw) {
            Ok(o) if o.status.success() => {
                report.push_str(&format!("\n  firewall: inbound UDP {port} allowed from tailnet addresses"));
            }
            _ => report
                .push_str(&format!("\n  firewall: not changed (needs an administrator shell); run:\n    {fw_cmd}")),
        }
        report.push_str("\nTailscale must run unattended (Tailscale menu > Preferences > Run unattended) to stay up when you log out.");
        Ok(report)
    }

    fn uninstall() -> io::Result<String> {
        let _ = schtasks(&["/End", "/TN", TASK]);
        let deleted = schtasks(&["/Delete", "/TN", TASK, "/F"])?;
        kill_others();
        let fw = run(
            "netsh.exe",
            &["advfirewall".into(), "firewall".into(), "delete".into(), "rule".into(), format!("name={FIREWALL_RULE}")],
        );
        let mut s = if deleted.status.success() {
            format!("removed scheduled task \"{TASK}\"")
        } else {
            format!("scheduled task \"{TASK}\" was not installed")
        };
        if !fw.is_ok_and(|o| o.status.success()) {
            s.push_str(&format!("\nfirewall rule \"{FIREWALL_RULE}\" not removed (needs an administrator shell)"));
        }
        Ok(s)
    }

    fn status() -> io::Result<String> {
        let q = schtasks(&["/Query", "/TN", TASK, "/V", "/FO", "LIST"])?;
        if !q.status.success() {
            return Ok(format!("scheduled task \"{TASK}\": not installed"));
        }
        let out = String::from_utf8_lossy(&q.stdout);
        let field = |k: &str| out.lines().find_map(|l| l.strip_prefix(k).map(|v| v.trim().to_owned()));
        let mut s = format!("scheduled task \"{TASK}\": installed");
        for (k, label) in [
            ("Status:", "state"),
            ("Last Run Time:", "last run"),
            ("Last Result:", "last result"),
            ("Run As User:", "user"),
        ] {
            if let Some(v) = field(k) {
                s.push_str(&format!("\n  {label}: {v}"));
            }
        }
        s.push_str(&format!("\n  process: {}", if process_running() { "running" } else { "not running" }));
        s.push_str(&format!("\n  logs: {}", log_path().display()));
        Ok(s)
    }
}
