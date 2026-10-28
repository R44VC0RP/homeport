<#
.SYNOPSIS
  Installs homeportd on Windows: copies the binary, configures shares, and
  registers it to run at boot as you (scheduled task, S4U: no stored password)
  with an inbound firewall rule for its UDP port from tailnet addresses.

.DESCRIPTION
  Run from an elevated PowerShell (S4U tasks and firewall rules need it).
  Safe to re-run: it replaces the binary, the task and the firewall rule.

.EXAMPLE
  # From a source checkout (builds with cargo):
  .\scripts\install-windows.ps1 -Share C:\Users\me\Projects -Share 'D:\Media=media'

.EXAMPLE
  # From a downloaded binary:
  .\install-windows.ps1 -Binary .\homeportd.exe -Share C:\Users\me\Projects

.EXAMPLE
  .\scripts\install-windows.ps1 -Uninstall
#>
[CmdletBinding()]
param(
    # homeportd.exe to install. Default: build it from this checkout.
    [string]$Binary,
    # Folders to share, as PATH or PATH=NAME. Adds to shares already configured.
    [string[]]$Share = @(),
    # UDP port (default 47820).
    [int]$Port = 0,
    # Extra Tailscale logins allowed to connect (default: this node's owner).
    [string[]]$AllowLogin = @(),
    # Remove the task, the firewall rule and the installed binary (keeps config and index).
    [switch]$Uninstall
)

$ErrorActionPreference = 'Stop'
$InstallDir = Join-Path $env:LOCALAPPDATA 'Programs\Homeport'
$Exe = Join-Path $InstallDir 'homeportd.exe'

# Runs a native command, returning its combined output. (Windows PowerShell
# turns a native program's stderr into errors under -ErrorAction Stop.)
function Invoke-Native {
    $ErrorActionPreference = 'Continue'
    $exe, $rest = $args
    & $exe @rest 2>&1 | ForEach-Object { "$_" } | Out-String
}

function Test-Admin {
    $p = [Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()
    $p.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
}

if (-not (Test-Admin)) {
    throw 'Run this from an elevated PowerShell (Run as administrator): the boot task and the firewall rule need it.'
}

if ($Uninstall) {
    if (Test-Path $Exe) {
        & $Exe service uninstall
    } else {
        Invoke-Native schtasks.exe /Delete /TN 'Homeport server' /F | Out-Null
        Invoke-Native netsh.exe advfirewall firewall delete rule 'name=Homeport server (homeportd, UDP)' | Out-Null
    }
    Get-Process homeportd -ErrorAction SilentlyContinue | Stop-Process -Force
    Remove-Item -Force -ErrorAction SilentlyContinue $Exe
    Write-Host "Removed homeportd. Config ($env:APPDATA\Homeport) and index ($env:LOCALAPPDATA\Homeport) were kept."
    return
}

# 1. The binary.
if (-not $Binary) {
    $root = Split-Path -Parent $PSScriptRoot
    if (-not (Test-Path (Join-Path $root 'Cargo.toml'))) {
        throw 'Pass -Binary <path to homeportd.exe>, or run this script from a Homeport source checkout.'
    }
    Write-Host 'Building homeportd (release)...'
    Push-Location $root
    try {
        $out = Invoke-Native cargo build --release -p homeportd
        if ($LASTEXITCODE) { throw "cargo build failed: $out" }
    } finally {
        Pop-Location
    }
    $Binary = Join-Path $root 'target\release\homeportd.exe'
}
$Binary = (Resolve-Path $Binary).Path
New-Item -ItemType Directory -Force $InstallDir | Out-Null
Get-Process homeportd -ErrorAction SilentlyContinue | Where-Object { $_.Path -eq $Exe } | Stop-Process -Force
Start-Sleep -Milliseconds 300
Copy-Item -Force $Binary $Exe
# A downloaded binary carries Mark-of-the-Web; a boot task can't answer SmartScreen.
Unblock-File $Exe
Write-Host "Installed $Exe"

# 2. Shares (in %APPDATA%\Homeport\server.toml).
foreach ($s in $Share) {
    $path, $name = $s -split '=', 2
    $full = (Resolve-Path $path).Path
    $listed = Invoke-Native $Exe share list
    $already = $listed -split "`n" | Where-Object { $_.Trim() -match ('\s(\\\\\?\\)?' + [regex]::Escape($full) + '$') }
    if ($already) {
        Write-Host "already shared: $($already.Trim())"
        continue
    }
    $a = @('share', 'add', $full)
    if ($name) { $a += @('--name', $name) }
    $out = Invoke-Native $Exe @a
    if ($LASTEXITCODE) { throw "homeportd share add $s failed: $out" }
    Write-Host $out.Trim()
}
$list = Invoke-Native $Exe share list
if ($LASTEXITCODE -or $list -match 'no shares;') { throw "No shares: pass -Share <folder>. $list" }
Write-Host $list.Trim()

# 3. Tailscale must keep running when nobody is logged on.
$ts = Join-Path $env:ProgramFiles 'Tailscale\tailscale.exe'
if (-not (Test-Path $ts)) {
    Write-Warning 'Tailscale is not installed (https://tailscale.com/download/windows). homeportd binds to the tailnet address.'
} else {
    $prefs = Invoke-Native $ts debug prefs
    if ($prefs -notmatch '"ForceDaemon":\s*true') {
        Write-Warning ('Tailscale is not in unattended mode, so it disconnects when you log out and homeportd ' +
            'becomes unreachable. Turn it on in the Tailscale menu: Preferences > Run unattended.')
    }
}

# 4. The boot task and the firewall rule.
$extra = @()
if ($Port) { $extra += @('--port', "$Port") }
foreach ($l in $AllowLogin) { $extra += @('--allow-login', $l) }
$svc = @('service', 'install')
if ($extra) { $svc += '--'; $svc += $extra }
$out = Invoke-Native $Exe @svc
if ($LASTEXITCODE) { throw "homeportd service install failed: $out" }
Write-Host $out.Trim()

# 5. Wait for the first start and show what clients need.
$fp = Join-Path $env:LOCALAPPDATA 'Homeport\server\cert.sha256'
for ($i = 0; $i -lt 30 -and -not (Test-Path $fp); $i++) { Start-Sleep -Milliseconds 500 }
Start-Sleep -Seconds 1
Write-Host (Invoke-Native $Exe service status).Trim()
if (Test-Path $fp) {
    $addr = if (Test-Path $ts) { (& $ts ip -4 | Select-Object -First 1) } else { '<tailnet IP>' }
    Write-Host ''
    Write-Host "Server:      $addr`:$(if ($Port) { $Port } else { 47820 })"
    Write-Host "Fingerprint: $((Get-Content $fp -Raw).Trim())"
} else {
    Write-Warning "homeportd has not started yet; see $env:LOCALAPPDATA\Homeport\logs\homeportd.log"
}
