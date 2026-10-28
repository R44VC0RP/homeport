<#
.SYNOPSIS
  Installs or upgrades the Homeport server (homeportd) on Windows: downloads
  the latest release, configures shares, and registers it to run at boot as
  you (scheduled task, S4U: no stored password) with an inbound firewall rule
  for its UDP port from tailnet addresses only.

.DESCRIPTION
  Run from an elevated PowerShell (S4U tasks and firewall rules need it).

  Safe to re-run: it replaces the binaries in place, never rewrites the
  config (shares are only ever added), and restarts an installed task with
  its existing arguments unless -Port or -AllowLogin is given.

  Environment: HOMEPORT_URL installs this zip (URL or local path) instead of
  the GitHub release.

.EXAMPLE
  # Latest release, asks for a folder to share:
  irm https://raw.githubusercontent.com/R44VC0RP/homeport/main/scripts/install-windows.ps1 | iex

.EXAMPLE
  # With arguments:
  & ([scriptblock]::Create((irm https://raw.githubusercontent.com/R44VC0RP/homeport/main/scripts/install-windows.ps1))) -Share C:\Users\me\Projects -Share 'D:\Media=media'

.EXAMPLE
  # From a source checkout (builds with cargo):
  .\scripts\install-windows.ps1 -Build -Share C:\Users\me\Projects

.EXAMPLE
  .\install-windows.ps1 -Uninstall
#>
[CmdletBinding()]
param(
    # Folders to share, as PATH or PATH=NAME. Adds to shares already configured.
    [string[]]$Share = @(),
    # Release to install, e.g. v0.6.0. Default: the latest.
    [string]$Version = 'latest',
    # homeportd.exe to install instead of downloading.
    [string]$Binary,
    # Build homeportd from the source checkout this script is in.
    [switch]$Build,
    # UDP port (default 47820).
    [int]$Port = 0,
    # Extra Tailscale logins allowed to connect (default: this node's owner).
    [string[]]$AllowLogin = @(),
    # Install the binaries and shares only.
    [switch]$NoService,
    # Remove the task, the firewall rule and the installed binaries (keeps config and index).
    [switch]$Uninstall
)

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
$Repo = 'R44VC0RP/homeport'
$Target = 'x86_64-pc-windows-msvc'
$TaskName = 'Homeport server'
$InstallDir = Join-Path $env:LOCALAPPDATA 'Programs\Homeport'
$Exe = Join-Path $InstallDir 'homeportd.exe'
$Cli = Join-Path $InstallDir 'homeport.exe'

# Runs a native command, returning its combined output. (Windows PowerShell
# turns a native program's stderr into errors under -ErrorAction Stop.)
function Invoke-Native {
    $ErrorActionPreference = 'Continue'
    # @(...): with one argument, `$exe, $rest = $args` makes $rest a string,
    # and splatting a string passes its characters.
    $exe = $args[0]
    $rest = @($args | Select-Object -Skip 1)
    & $exe @rest 2>&1 | ForEach-Object { "$_" } | Out-String
}

function Test-Admin {
    $p = [Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()
    $p.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
}

function Test-TaskInstalled {
    $null = Invoke-Native schtasks.exe /Query /TN $TaskName
    $LASTEXITCODE -eq 0
}

function Test-HasShare {
    $list = Invoke-Native $Exe share list
    ($LASTEXITCODE -eq 0) -and ($list -notmatch 'no shares;')
}

function Add-Share([string]$Spec) {
    $path, $name = $Spec -split '=', 2
    $full = (Resolve-Path $path).Path
    $listed = Invoke-Native $Exe share list
    $already = $listed -split "`n" | Where-Object { $_.Trim() -match ('\s(\\\\\?\\)?' + [regex]::Escape($full) + '$') }
    if ($already) {
        Write-Host "already shared: $($already.Trim())"
        return
    }
    $a = @('share', 'add', $full)
    if ($name) { $a += @('--name', $name) }
    $out = Invoke-Native $Exe @a
    if ($LASTEXITCODE) { throw "homeportd share add $Spec failed: $out" }
    Write-Host $out.Trim()
}

if (-not (Test-Admin)) {
    throw 'Run this from an elevated PowerShell (Run as administrator): the boot task and the firewall rule need it.'
}

if ($Uninstall) {
    if (Test-Path $Exe) {
        Write-Host (Invoke-Native $Exe service uninstall).Trim()
    } else {
        Invoke-Native schtasks.exe /Delete /TN $TaskName /F | Out-Null
        Invoke-Native netsh.exe advfirewall firewall delete rule 'name=Homeport server (homeportd, UDP)' | Out-Null
    }
    Get-Process homeportd -ErrorAction SilentlyContinue | Stop-Process -Force
    Remove-Item -Force -ErrorAction SilentlyContinue $Exe, $Cli
    Write-Host "Removed homeportd. Config ($env:APPDATA\Homeport) and index ($env:LOCALAPPDATA\Homeport) were kept."
    return
}

# 1. Get the binaries.
$tmp = Join-Path ([IO.Path]::GetTempPath()) ("homeport-" + [guid]::NewGuid())
New-Item -ItemType Directory -Force $tmp | Out-Null
try {
    $newCli = $null
    if ($Build) {
        $root = if ($PSScriptRoot) { Split-Path -Parent $PSScriptRoot } else { '' }
        if (-not $root -or -not (Test-Path (Join-Path $root 'Cargo.toml'))) {
            throw '-Build must run from a Homeport source checkout.'
        }
        Write-Host 'Building homeportd (release)...'
        Push-Location $root
        try {
            $out = Invoke-Native cargo build --release --locked -p homeportd -p homeport-cli
            if ($LASTEXITCODE) { throw "cargo build failed: $out" }
        } finally {
            Pop-Location
        }
        $Binary = Join-Path $root 'target\release\homeportd.exe'
        $newCli = Join-Path $root 'target\release\homeport.exe'
    } elseif (-not $Binary) {
        # GitHub needs TLS 1.2, which Windows PowerShell 5.1 may not offer by default.
        [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
        $asset = "homeport-$Target.zip"
        $zip = Join-Path $tmp $asset
        if ($env:HOMEPORT_URL) {
            $url = $env:HOMEPORT_URL
        } elseif ($Version -eq 'latest') {
            $url = "https://github.com/$Repo/releases/latest/download/$asset"
        } else {
            $url = "https://github.com/$Repo/releases/download/$Version/$asset"
        }
        Write-Host "Downloading $url"
        if (Test-Path -LiteralPath $url) {
            Copy-Item -LiteralPath $url $zip
        } else {
            Invoke-WebRequest -UseBasicParsing -Uri $url -OutFile $zip
            if (-not $env:HOMEPORT_URL) {
                $sumsUrl = $url.Substring(0, $url.LastIndexOf('/')) + '/SHA256SUMS'
                $sums = (Invoke-WebRequest -UseBasicParsing -Uri $sumsUrl).Content
                if ($sums -is [byte[]]) { $sums = [Text.Encoding]::UTF8.GetString($sums) }
                $line = $sums -split "`n" | Where-Object { $_ -match (' ' + [regex]::Escape($asset) + '\s*$') } | Select-Object -First 1
                if (-not $line) { throw "$asset is not listed in SHA256SUMS" }
                $want = ($line -split '\s+')[0]
                $got = (Get-FileHash -Algorithm SHA256 $zip).Hash
                if ($got -ne $want) { throw "Checksum mismatch for $asset" }
            }
        }
        Expand-Archive -Force $zip $tmp
        $Binary = Join-Path $tmp "homeport-$Target\homeportd.exe"
        $newCli = Join-Path $tmp "homeport-$Target\homeport.exe"
    }
    $Binary = (Resolve-Path $Binary).Path

    # 2. Install them. Stop the running server first: Windows can't replace a running exe.
    $taskInstalled = Test-TaskInstalled
    New-Item -ItemType Directory -Force $InstallDir | Out-Null
    if ($taskInstalled) { Invoke-Native schtasks.exe /End /TN $TaskName | Out-Null }
    Get-Process homeportd -ErrorAction SilentlyContinue | Where-Object { $_.Path -eq $Exe } | Stop-Process -Force
    Start-Sleep -Milliseconds 500
    Copy-Item -Force $Binary $Exe
    # A downloaded binary carries Mark-of-the-Web; a boot task can't answer SmartScreen.
    Unblock-File $Exe
    if ($newCli -and (Test-Path $newCli)) {
        Copy-Item -Force $newCli $Cli
        Unblock-File $Cli
    }
    Write-Host "Installed $((Invoke-Native $Exe --version).Trim()) in $InstallDir"
} finally {
    Remove-Item -Recurse -Force -ErrorAction SilentlyContinue $tmp
}

# 3. Shares (in %APPDATA%\Homeport\server.toml). Only ever added.
foreach ($s in $Share) { Add-Share $s }
if (-not (Test-HasShare) -and [Environment]::UserInteractive -and $Host.Name -eq 'ConsoleHost') {
    $answer = Read-Host 'Folder to share with your Mac (empty to skip)'
    if ($answer) { Add-Share $answer }
}
if (-not (Test-HasShare)) {
    Write-Host ''
    Write-Host 'No folders are shared yet. Share one and start the server (elevated):'
    Write-Host "  & '$Exe' share add <folder>"
    Write-Host "  & '$Exe' service install"
    return
}
Write-Host (Invoke-Native $Exe share list).Trim()

# 4. Tailscale must keep running when nobody is logged on.
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

# 5. The boot task and the firewall rule.
if ($NoService) {
    Write-Host "Skipped the service (-NoService); start it with: & '$Exe' service install"
} elseif ($taskInstalled -and -not $Port -and -not $AllowLogin) {
    # An upgrade: keep the task (and the arguments it was registered with).
    $out = Invoke-Native schtasks.exe /Run /TN $TaskName
    if ($LASTEXITCODE) { throw "schtasks /Run failed: $out" }
    Write-Host "Restarted the scheduled task '$TaskName'."
} else {
    $extra = @()
    if ($Port) { $extra += @('--port', "$Port") }
    foreach ($l in $AllowLogin) { $extra += @('--allow-login', $l) }
    $svc = @('service', 'install')
    if ($extra) { $svc += '--'; $svc += $extra }
    $out = Invoke-Native $Exe @svc
    if ($LASTEXITCODE) { throw "homeportd service install failed: $out" }
    Write-Host $out.Trim()
}

# 6. Wait for the first start, then pair.
$fp = Join-Path $env:LOCALAPPDATA 'Homeport\server\cert.sha256'
if (-not $NoService) {
    for ($i = 0; $i -lt 30 -and -not (Test-Path $fp); $i++) { Start-Sleep -Milliseconds 500 }
    Start-Sleep -Seconds 1
    Write-Host (Invoke-Native $Exe service status).Trim()
    if (-not (Test-Path $fp)) {
        Write-Warning "homeportd has not started yet; see $env:LOCALAPPDATA\Homeport\logs\homeportd.log"
    }
}
Write-Host ''
$null = Invoke-Native $Exe pair --help
if ($LASTEXITCODE -eq 0) {
    Write-Host "Next: run '& `"$Exe`" pair' and open the link it prints on your Mac (or enter its 6-digit code in Homeport)."
} else {
    Write-Host 'Next: add this server in the Homeport app on your Mac.'
    $addr = if (Test-Path $ts) { (& $ts ip -4 | Select-Object -First 1) } else { '<tailnet IP>' }
    Write-Host "  address:     $addr`:$(if ($Port) { $Port } else { 47820 })"
    if (Test-Path $fp) { Write-Host "  fingerprint: $((Get-Content $fp -Raw).Trim())" }
}
