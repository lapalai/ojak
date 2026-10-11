<#
.SYNOPSIS
Installs Ojak on Windows 10 1809+/11 x64 from a public GitHub release.

.DESCRIPTION
Run as the ordinary user in Windows PowerShell 5.1 or PowerShell 7 on native
Windows. Do not run as Administrator. The script:

  1. resolves ONE public release of lapalai/ojak (latest, or -Version) and pins it;
  2. requires exactly the x64 NSIS installer and SHA256SUMS release assets at the
     exact https://github.com/lapalai/ojak/releases/download/<tag>/<name> URLs;
  3. downloads the installer, verifies its size and SHA-256, and runs the normal
     NSIS installer silently (current-user install, %LOCALAPPDATA%\Ojak by default).
     The installer's own safety hooks (installer prepare / finish / recover) run
     unchanged: it stops when managed sessions are active or uncertain;
  4. runs the NEWLY INSTALLED aam.exe by absolute path: "aam setup", then
     "aam setup --check", and evaluates the JSON result.

Nothing here disables SmartScreen, antivirus, execution policy, or any approval
control, installs Node/Bun/runtimes, builds from source, or force-closes running
programs. The Windows installer is NOT code-signed (early preview). The
SHA256SUMS check detects a corrupted or mismatched download; it is not a
publisher signature.

Exit codes: 0 ready; 1 refused or failed before anything was installed;
2 installer failed; 3 installed but setup incomplete (login, verification,
service version, or setup error); 4 installed and ready, but the requested omp
integration is not fully connected.

.PARAMETER Version
Public release to install, for example 0.5.8 (a leading v is accepted).
Default: the latest public release, resolved once.

.PARAMETER WithOmp
Explicit opt-in to connect an EXISTING omp installation ("aam setup --with-omp").
It relays subscription credentials through a local proxy and may conflict with
provider terms. It does not install omp.

.PARAMETER DryRun
Print the plan only. Reads public release metadata (and SHA256SUMS into memory)
but downloads no installer, writes nothing, and changes no service.

.PARAMETER Help
Print usage and exit.

.EXAMPLE
.\install.ps1 -DryRun

.EXAMPLE
.\install.ps1 -Version 0.5.8 -WithOmp
#>
[CmdletBinding()]
param(
    [string]$Version,
    [switch]$WithOmp,
    [switch]$DryRun,
    [switch]$Help
)

Set-StrictMode -Version 2.0
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

# OJAK_REPO is the only GitHub owner/repo in this file.
$script:Repo = 'lapalai/ojak'
$script:ApiBase = 'https://api.github.com/repos/' + $script:Repo + '/releases'
$script:DownloadPrefix = 'https://github.com/' + $script:Repo + '/releases/download/'
$script:UserAgent = 'ojak-install.ps1'
$script:MaxInstallerBytes = 314572800
$script:MaxSumsBytes = 65536
$script:TempDir = $null
$script:BlockingNoticeCodes = @('service-missing', 'service-version-mismatch', 'missing-shim', 'path-shadow', 'unsupported-shell', 'windows-mismatch', 'command-mismatch')

function Show-OjakHelp {
    Write-Host @'
Usage: .\install.ps1 [-Version <x.y.z>] [-WithOmp] [-DryRun] [-Help]

Installs Ojak on Windows 10 1809+/11 x64 (current user, %LOCALAPPDATA%\Ojak),
then runs the installed aam.exe: "aam setup" and "aam setup --check".

  -Version <x.y.z>  install this public release (default: latest, resolved once)
  -WithOmp          explicit opt-in: also connect an EXISTING omp installation
                    ("aam setup --with-omp"). It relays subscription credentials
                    through a local proxy and may conflict with provider terms.
                    This does not install omp.
  -DryRun           print the plan only: no installer download, no writes, no
                    service changes
  -Help             show this help

Run as the ordinary user, not Administrator. Quit Orca and omp first; the script
asks Ojak's own window to close but never force-stops anything. The installer
stops by itself when managed sessions are active or uncertain.

The Windows installer is not code-signed (early preview). The SHA256SUMS check
detects a corrupt or mismatched download; it is not a publisher signature.
This script does not disable SmartScreen, antivirus, or execution policy.

Exit codes:
  0  installed and ready
  1  refused or failed before anything was installed
  2  installer failed (existing files are not replaced when its safety check stops)
  3  installed, setup incomplete (login, verification, service version, setup error)
  4  installed and ready, requested omp integration not fully connected
'@
}

# ---------------------------------------------------------------- helpers

function Write-Ojak {
    param([string]$Message)
    Write-Host ('[ojak] ' + $Message)
}

function Stop-Ojak {
    param([int]$Code, [string]$Status, [string]$Message)
    $ex = [System.Management.Automation.RuntimeException]::new($Message)
    $ex.Data['OjakExitCode'] = $Code
    $ex.Data['OjakStatus'] = $Status
    throw $ex
}

function Get-Prop {
    param($Object, [string]$Name)
    if ($null -eq $Object) { return $null }
    $p = $Object.PSObject.Properties[$Name]
    if ($null -eq $p) { return $null }
    return $p.Value
}

function Test-Has {
    param($Object, [string]$Name)
    if ($null -eq $Object) { return $false }
    return ($null -ne $Object.PSObject.Properties[$Name])
}

# Output that may come from tools or the network must not carry secrets or contact data.
function Protect-Text {
    param([string]$Text, [int]$Max = 400)
    if ($null -eq $Text) { return '' }
    $t = [regex]::Replace($Text, '[\x00-\x08\x0B\x0C\x0E-\x1F]', '')
    $t = [regex]::Replace($t, '[A-Za-z0-9._%+\-]+@[A-Za-z0-9.\-]+\.[A-Za-z]{2,}', '<email>')
    $t = [regex]::Replace($t, '(?i)\bbearer\s+\S+', 'bearer <redacted>')
    $t = [regex]::Replace($t, '\b(?:sk|pk|ghp|gho|ghu|ghs|github_pat|xox[a-z])[-_A-Za-z0-9]{16,}', '<token>')
    $t = [regex]::Replace($t, '\b[A-Za-z0-9_\-]{40,}\b', '<redacted>')
    if ($t.Length -gt $Max) { $t = $t.Substring(0, $Max) + '...' }
    return $t
}

function ConvertTo-PlainText {
    param($Content)
    if ($null -eq $Content) { return '' }
    if ($Content -is [byte[]]) { return [System.Text.Encoding]::UTF8.GetString($Content) }
    return [string]$Content
}

# ---------------------------------------------------------------- platform

function Assert-SupportedPlatform {
    if ($PSVersionTable.PSVersion -lt [version]'5.1') {
        Stop-Ojak 1 'not-installed' 'Windows PowerShell 5.1 or newer is required. Nothing was installed.'
    }
    if ([Environment]::OSVersion.Platform -ne [PlatformID]::Win32NT) {
        Stop-Ojak 1 'not-installed' 'This installer is for native Windows. Inside WSL or a container, target the Windows host through an authorized Windows shell instead. Nothing was installed.'
    }
    $arch = $env:PROCESSOR_ARCHITEW6432
    if ([string]::IsNullOrEmpty($arch)) { $arch = $env:PROCESSOR_ARCHITECTURE }
    if ($arch -ne 'AMD64') {
        Stop-Ojak 1 'not-installed' ('This release is for Windows x64. Detected architecture: ' + (Protect-Text ([string]$arch) 20) + '. Nothing was installed.')
    }
    if ([Environment]::OSVersion.Version.Build -lt 17763) {
        Stop-Ojak 1 'not-installed' 'Windows 10 version 1809 (build 17763) or newer is required. Nothing was installed.'
    }
    if ([string]::IsNullOrEmpty($env:LOCALAPPDATA)) {
        Stop-Ojak 1 'not-installed' 'LOCALAPPDATA is not set; cannot locate the current-user install folder. Nothing was installed.'
    }
}

function Test-Elevated {
    # Seam for the behavioral tests (tests/install-ps1.tests.ps1): they run this exact
    # script in a child session that defines Get-OjakTestElevation. Defining it needs
    # code execution in this session already, so it grants nothing.
    $hook = Get-Command -Name 'Get-OjakTestElevation' -CommandType Function -ErrorAction SilentlyContinue
    if ($hook) { return [bool](& $hook) }
    $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
    $principal = New-Object -TypeName Security.Principal.WindowsPrincipal -ArgumentList $identity
    return [bool]$principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
}

# ---------------------------------------------------------------- release

function Invoke-WebText {
    param([string]$Uri, [bool]$Api, [string]$What)
    try {
        if ($Api) {
            $response = Invoke-WebRequest -UseBasicParsing -Uri $Uri -UserAgent $script:UserAgent -TimeoutSec 60 -MaximumRedirection 0 -Headers @{ Accept = 'application/vnd.github+json'; 'X-GitHub-Api-Version' = '2022-11-28' }
        } else {
            $response = Invoke-WebRequest -UseBasicParsing -Uri $Uri -UserAgent $script:UserAgent -TimeoutSec 60
        }
    } catch {
        $hint = ''
        if ($_.Exception.Message -match '403|429|rate limit') { $hint = ' GitHub may be rate limiting this network; retry later.' }
        Stop-Ojak 1 'not-installed' ('Could not read ' + $What + ' from GitHub.' + $hint + ' Nothing was installed.')
    }
    # Read .Content directly: a byte[] (octet-stream SHA256SUMS) must not be unrolled by a function return.
    return (ConvertTo-PlainText $response.Content)
}

function Resolve-Release {
    param([string]$RequestedVersion)
    if ($RequestedVersion) { $uri = $script:ApiBase + '/tags/v' + $RequestedVersion } else { $uri = $script:ApiBase + '/latest' }
    $text = Invoke-WebText -Uri $uri -Api $true -What 'the release metadata'
    try { $rel = ConvertFrom-Json -InputObject $text } catch { $rel = $null }
    if ($null -eq $rel -or $rel.GetType().Name -ne 'PSCustomObject') {
        Stop-Ojak 1 'not-installed' 'The release metadata is not a valid JSON object. Nothing was installed.'
    }

    $tag = Get-Prop $rel 'tag_name'
    if ($tag -isnot [string] -or $tag -cnotmatch '^v([0-9]{1,6})\.([0-9]{1,6})\.([0-9]{1,6})$') {
        Stop-Ojak 1 'not-installed' 'The release tag is missing or is not vX.Y.Z. Nothing was installed.'
    }
    $ver = $tag.Substring(1)
    if ($RequestedVersion -and $ver -cne $RequestedVersion) {
        Stop-Ojak 1 'not-installed' ('GitHub returned tag ' + $tag + ' for requested version ' + $RequestedVersion + '. Nothing was installed.')
    }
    if ((Get-Prop $rel 'draft') -isnot [bool] -or (Get-Prop $rel 'draft') -ne $false) {
        Stop-Ojak 1 'not-installed' ('Release ' + $tag + ' is a draft. Nothing was installed.')
    }
    if ((Get-Prop $rel 'prerelease') -isnot [bool] -or (Get-Prop $rel 'prerelease') -ne $false) {
        Stop-Ojak 1 'not-installed' ('Release ' + $tag + ' is a prerelease. Nothing was installed.')
    }

    $assets = @(Get-Prop $rel 'assets')
    $installerName = 'Ojak_' + $ver + '_x64-setup.exe'
    $picked = @{}
    foreach ($name in @($installerName, 'SHA256SUMS')) {
        $matching = @($assets | Where-Object { (Get-Prop $_ 'name') -ceq $name })
        if ($matching.Count -ne 1) {
            Stop-Ojak 1 'not-installed' ('Release ' + $tag + ' must contain exactly one asset named ' + $name + ' (found ' + $matching.Count + '). Nothing was installed.')
        }
        $asset = $matching[0]
        $expectedUrl = $script:DownloadPrefix + $tag + '/' + $name
        if ((Get-Prop $asset 'browser_download_url') -isnot [string] -or (Get-Prop $asset 'browser_download_url') -cne $expectedUrl) {
            Stop-Ojak 1 'not-installed' ('Refusing ' + $name + ': its URL is not exactly ' + $expectedUrl + '. Nothing was installed.')
        }
        if ((Get-Prop $asset 'state') -cne 'uploaded') {
            Stop-Ojak 1 'not-installed' ('Asset ' + $name + ' is not in the uploaded state. Nothing was installed.')
        }
        $size = Get-Prop $asset 'size'
        $limit = $script:MaxInstallerBytes
        if ($name -ceq 'SHA256SUMS') { $limit = $script:MaxSumsBytes }
        if (($size -isnot [int] -and $size -isnot [long]) -or $size -le 0 -or $size -gt $limit) {
            Stop-Ojak 1 'not-installed' ('Asset ' + $name + ' has a missing or implausible size. Nothing was installed.')
        }
        $digest = Get-Prop $asset 'digest'
        if ($null -ne $digest) {
            if ($digest -isnot [string] -or $digest -cnotmatch '^sha256:[0-9a-f]{64}$') {
                Stop-Ojak 1 'not-installed' ('Asset ' + $name + ' has a malformed digest. Nothing was installed.')
            }
            $digest = $digest.Substring(7)
        }
        $picked[$name] = [pscustomobject]@{ Name = $name; Url = $expectedUrl; Size = [long]$size; Digest = $digest }
    }
    return [pscustomobject]@{ Tag = $tag; Version = $ver; Installer = $picked[$installerName]; Sums = $picked['SHA256SUMS'] }
}

function Get-ExpectedHash {
    param([string]$SumsText, [string]$Name)
    $found = @()
    $clean = $SumsText.TrimStart([char]0xFEFF)
    foreach ($line in ($clean -split "`n")) {
        $l = $line.TrimEnd("`r")
        if ($l.Length -eq 0) { continue }
        if ($l -cmatch '^([0-9a-f]{64})  ([^\s/\\]+)$') {
            if ($Matches[2] -ceq $Name) { $found += $Matches[1] }
        } else {
            Stop-Ojak 1 'not-installed' 'SHA256SUMS is malformed (expected "<64 hex>  <name>" lines). Nothing was installed.'
        }
    }
    if ($found.Count -ne 1) {
        Stop-Ojak 1 'not-installed' ('SHA256SUMS must list ' + $Name + ' exactly once (found ' + $found.Count + '). Nothing was installed.')
    }
    return $found[0]
}

# ---------------------------------------------------------------- installed state

function Read-RegistryKey {
    param([string]$Path)
    try { return (Get-ItemProperty -LiteralPath $Path -ErrorAction SilentlyContinue) } catch { return $null }
}

function Get-InstalledState {
    $manu = Read-RegistryKey 'HKCU:\Software\lapal\Ojak'
    $uninst = Read-RegistryKey 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\Ojak'
    $dir = $null
    $registered = Get-Prop $manu '(default)'
    if ($registered -is [string] -and $registered.Trim().Length -gt 0) { $dir = $registered.Trim().Trim('"') }
    if (-not $dir) { $dir = Join-Path $env:LOCALAPPDATA 'Ojak' }
    if ($dir -match '[*?<>|"]' -or -not [IO.Path]::IsPathRooted($dir)) {
        Stop-Ojak 1 'not-installed' 'The registered Ojak install folder is not a plain absolute path. Nothing was installed.'
    }
    $dir = [IO.Path]::GetFullPath($dir).TrimEnd('\')
    $ver = $null
    $display = Get-Prop $uninst 'DisplayVersion'
    if ($display -is [string] -and $display -match '^[0-9]+\.[0-9]+\.[0-9]+$') { $ver = [version]$display }
    $location = Get-Prop $uninst 'InstallLocation'
    if ($location -is [string]) { $location = $location.Trim().Trim('"').TrimEnd('\') } else { $location = $null }
    return [pscustomobject]@{
        Dir = $dir
        Aam = (Join-Path $dir 'aam.exe')
        Version = $ver
        Location = $location
        Present = ($null -ne $uninst)
    }
}

# ---------------------------------------------------------------- processes

function Get-OjakProcesses {
    param([string]$Dir)
    try {
        $table = @(Get-CimInstance -ClassName Win32_Process -Property ProcessId, ParentProcessId, Name, ExecutablePath -ErrorAction Stop)
    } catch {
        Stop-Ojak 1 'not-installed' 'Could not list running programs to check for locked Ojak files. Nothing was installed.'
    }
    $byId = @{}
    foreach ($p in $table) { $byId[[int]$p.ProcessId] = $p }
    $ours = @{}
    $cur = $PID
    $guard = 0
    while ($byId.ContainsKey($cur) -and $guard -lt 64) {
        $ours[$cur] = $true
        $cur = [int]$byId[$cur].ParentProcessId
        $guard++
    }
    $watch = @('aam.exe', 'ai-account-manager.exe', 'ojak.exe')
    $found = @()
    foreach ($p in $table) {
        $exe = $p.ExecutablePath
        if ([string]::IsNullOrEmpty($exe)) { continue }
        if ($exe.StartsWith('\\?\')) { $exe = $exe.Substring(4) }
        $name = [IO.Path]::GetFileName($exe).ToLowerInvariant()
        if ($watch -notcontains $name) { continue }
        $parentDir = [IO.Path]::GetDirectoryName($exe)
        if (-not [string]::Equals($parentDir.TrimEnd('\'), $Dir.TrimEnd('\'), [StringComparison]::OrdinalIgnoreCase)) { continue }
        $parentName = '?'
        if ($byId.ContainsKey([int]$p.ParentProcessId)) { $parentName = [string]$byId[[int]$p.ParentProcessId].Name }
        $found += [pscustomobject]@{
            Id = [int]$p.ProcessId
            Name = $name
            Gui = ($name -ne 'aam.exe')
            Parent = $parentName
            InOurSession = $ours.ContainsKey([int]$p.ProcessId)
        }
    }
    return $found
}

function Format-Blockers {
    param($Blockers)
    return (($Blockers | ForEach-Object { $_.Name + ' (PID ' + $_.Id + ', started by ' + $_.Parent + ')' }) -join '; ')
}

# Asks Ojak's own window to close. Never terminates anything. Throws when files stay locked.
function Request-OjakRelease {
    param([string]$Dir)
    $blockers = @(Get-OjakProcesses $Dir)
    if ($blockers.Count -eq 0) { return }
    $mine = @($blockers | Where-Object { $_.InOurSession })
    if ($mine.Count -gt 0) {
        Stop-Ojak 1 'not-installed' ('This script is running inside an Ojak-managed session (' + (Format-Blockers $mine) + '). Run it from a separate terminal outside Ojak, omp, and Orca. Nothing was installed.')
    }
    $gui = @($blockers | Where-Object { $_.Gui })
    if ($gui.Count -gt 0) {
        Write-Ojak 'Asking the running Ojak app to close (no force-stop)...'
        foreach ($g in $gui) {
            $proc = Get-Process -Id $g.Id -ErrorAction SilentlyContinue
            if ($proc -and $proc.MainWindowHandle -ne [IntPtr]::Zero) { [void]$proc.CloseMainWindow() }
        }
        $deadline = (Get-Date).AddSeconds(30)
        do {
            Start-Sleep -Milliseconds 500
            $blockers = @(Get-OjakProcesses $Dir)
            $gui = @($blockers | Where-Object { $_.Gui })
        } while ($gui.Count -gt 0 -and (Get-Date) -lt $deadline)
        if ($blockers.Count -gt 0 -and $gui.Count -eq 0) {
            $deadline = (Get-Date).AddSeconds(5)
            do {
                Start-Sleep -Milliseconds 500
                $blockers = @(Get-OjakProcesses $Dir)
            } while ($blockers.Count -gt 0 -and (Get-Date) -lt $deadline)
        }
    }
    if ($blockers.Count -gt 0) {
        Stop-Ojak 1 'not-installed' ('Ojak files are in use: ' + (Format-Blockers $blockers) + '. Quit Ojak (tray menu), Orca, and every omp process normally, finish running Claude Code/Codex work, then run this script again. Nothing was force-stopped and nothing was installed.')
    }
}

# ---------------------------------------------------------------- external programs

# Runs a program without a shell, captures output, and never kills it on timeout.
function Invoke-External {
    param([string]$File, [string]$Arguments, [int]$TimeoutSec, [bool]$Capture)
    $psi = New-Object -TypeName System.Diagnostics.ProcessStartInfo
    $psi.FileName = $File
    $psi.Arguments = $Arguments
    $psi.UseShellExecute = $false
    $psi.CreateNoWindow = $true
    if ($Capture) {
        $psi.RedirectStandardOutput = $true
        $psi.RedirectStandardError = $true
        $psi.StandardOutputEncoding = [System.Text.Encoding]::UTF8
        $psi.StandardErrorEncoding = [System.Text.Encoding]::UTF8
    }
    try {
        $p = [System.Diagnostics.Process]::Start($psi)
    } catch {
        return [pscustomobject]@{ Started = $false; TimedOut = $false; ExitCode = $null; Stdout = ''; Stderr = (Protect-Text $_.Exception.Message); Pid = 0 }
    }
    $outTask = $null
    $errTask = $null
    if ($Capture) {
        $outTask = $p.StandardOutput.ReadToEndAsync()
        $errTask = $p.StandardError.ReadToEndAsync()
    }
    $exited = $p.WaitForExit($TimeoutSec * 1000)
    if (-not $exited) {
        return [pscustomobject]@{ Started = $true; TimedOut = $true; ExitCode = $null; Stdout = ''; Stderr = ''; Pid = $p.Id }
    }
    $p.WaitForExit()
    $stdout = ''
    $stderr = ''
    if ($Capture) { $stdout = $outTask.Result; $stderr = $errTask.Result }
    return [pscustomobject]@{ Started = $true; TimedOut = $false; ExitCode = $p.ExitCode; Stdout = $stdout; Stderr = $stderr; Pid = $p.Id }
}

function Get-AamVersion {
    param([string]$Aam)
    if (-not (Test-Path -LiteralPath $Aam)) { return $null }
    $r = Invoke-External -File $Aam -Arguments '--version' -TimeoutSec 30 -Capture $true
    if (-not $r.Started -or $r.TimedOut -or $r.ExitCode -ne 0) { return $null }
    if ($r.Stdout.Trim() -cmatch '^aam ([0-9]+\.[0-9]+\.[0-9]+)$') { return $Matches[1] }
    return $null
}

# ---------------------------------------------------------------- setup evaluation

function Read-SetupJson {
    param($Result, [string]$What)
    if (-not $Result.Started -or $Result.TimedOut) { return $null }
    try { $obj = ConvertFrom-Json -InputObject $Result.Stdout } catch { return $null }
    if ($null -eq $obj -or $obj.GetType().Name -ne 'PSCustomObject') { return $null }
    return $obj
}

function Get-ErrorCode {
    param([string]$Text)
    if ($Text -match '\(([A-Z][A-Z0-9_]{2,})\)\s*$') { return $Matches[1] }
    return $null
}

# Returns Problems (blocking), Warnings, NeedsLogin for an `aam setup --check` object.
function Test-CheckResult {
    param($Check)
    $problems = New-Object System.Collections.Generic.List[string]
    $warnings = New-Object System.Collections.Generic.List[string]
    $needsLogin = $false
    foreach ($field in @('ready', 'service', 'shell', 'configured', 'serviceVersionMismatch')) {
        if ((Get-Prop $Check $field) -isnot [bool]) { $problems.Add('malformed result: "' + $field + '" is missing or not true/false') }
    }
    $tools = @(Get-Prop $Check 'tools')
    if (-not (Test-Has $Check 'tools') -or $tools.Count -eq 0) { $problems.Add('malformed result: "tools" is missing or empty') }
    $notices = @(Get-Prop $Check 'notices')
    if (-not (Test-Has $Check 'notices')) { $problems.Add('malformed result: "notices" is missing') }
    if ($problems.Count -gt 0) {
        return [pscustomobject]@{ Problems = @($problems); Warnings = @(); NeedsLogin = $false; Tools = @(); Notices = @() }
    }
    if ((Get-Prop $Check 'ready') -ne $true) { $problems.Add('ready is false') }
    if ((Get-Prop $Check 'serviceVersionMismatch') -ne $false) { $problems.Add('the running service version does not match the installed app (serviceVersionMismatch)') }
    if ((Get-Prop $Check 'service') -ne $true) { $problems.Add('the background service is not running') }
    $anyAccount = $false
    foreach ($tool in $tools) {
        $name = Protect-Text ([string](Get-Prop $tool 'tool')) 20
        $account = Get-Prop $tool 'account'
        if ($account -eq $true) {
            $anyAccount = $true
            if ((Get-Prop $tool 'connected') -ne $true) { $problems.Add($name + ': command connection (shim) is missing') }
            if ((Get-Prop $tool 'verified') -ne $true) {
                $why = Get-Prop $tool 'verificationError'
                $detail = ''
                if ($why -is [string] -and $why) { $detail = ': ' + (Protect-Text $why 200) }
                $problems.Add($name + ': new-terminal command check did not pass' + $detail)
            }
        }
    }
    if (-not $anyAccount) { $needsLogin = $true; $problems.Add('no usable Claude Code or Codex account was found') }
    foreach ($notice in $notices) {
        $code = [string](Get-Prop $notice 'code')
        $text = $code + ' (' + (Protect-Text ([string](Get-Prop $notice 'step')) 30) + '): ' + (Protect-Text ([string](Get-Prop $notice 'message')) 300)
        if ($script:BlockingNoticeCodes -contains $code) { $problems.Add('notice ' + $text) } else { $warnings.Add('notice ' + $text) }
    }
    return [pscustomobject]@{ Problems = @($problems); Warnings = @($warnings); NeedsLogin = $needsLogin; Tools = $tools; Notices = $notices }
}

function Get-OmpState {
    param($Omp)
    if ($null -eq $Omp -or $Omp.GetType().Name -ne 'PSCustomObject') {
        return [pscustomobject]@{ Present = $false; Connected = $false; Detail = 'no omp result (omp was not detected or the result was missing)' }
    }
    $parts = @()
    foreach ($field in @('broker', 'bridge', 'observer')) {
        $v = Get-Prop $Omp $field
        if ($v -eq $true) { $parts += ($field + '=connected') } else { $parts += ($field + '=NOT connected') }
    }
    $err = Get-Prop $Omp 'error'
    $connected = ((Get-Prop $Omp 'broker') -eq $true) -and ((Get-Prop $Omp 'bridge') -eq $true) -and ((Get-Prop $Omp 'observer') -eq $true) -and ($null -eq $err)
    $detail = $parts -join ', '
    if ($null -ne $err) { $detail += '; error: ' + (Protect-Text ([string]$err) 300) }
    return [pscustomobject]@{ Present = $true; Connected = $connected; Detail = $detail }
}

# ---------------------------------------------------------------- reporting

function Write-Result {
    param([string]$Status, [string]$Next)
    Write-Host ''
    if ($Next) { Write-Ojak ('Next: ' + $Next) }
    Write-Host ('RESULT: ' + $Status)
}

function Write-Disclaimers {
    param([bool]$Omp)
    Write-Ojak 'Ojak is an unofficial early preview. It is not affiliated with Anthropic, OpenAI, Google, or xAI. Use only your own accounts.'
    Write-Ojak 'The Windows installer is NOT code-signed. Windows SmartScreen or antivirus may warn. This script does not disable them or change execution policy.'
    Write-Ojak 'SHA256SUMS comes from the same GitHub release: it detects a corrupt or mismatched download, not a malicious publisher. Updater signatures are checked only by the in-app updater.'
    if ($Omp) {
        Write-Ojak 'omp (opt-in): the bridge routes subscription credentials through a local proxy and may conflict with provider terms (https://code.claude.com/docs/en/legal-and-compliance, https://openai.com/policies/row-terms-of-use/). You are responsible for how you use it; account suspension is possible. This does not install omp.'
    }
}

# ---------------------------------------------------------------- main

function Invoke-OjakMain {
    if ($Help) { Show-OjakHelp; return 0 }

    $requested = $null
    if ($Version) {
        if ($Version -cmatch '^v?([0-9]{1,6}\.[0-9]{1,6}\.[0-9]{1,6})$') {
            $requested = $Matches[1]
        } else {
            Stop-Ojak 1 'not-installed' '-Version must look like 0.5.8 (digits and dots only). Nothing was installed.'
        }
    }

    Assert-SupportedPlatform
    # Windows PowerShell 5.1 may default to protocols GitHub refuses. This only adds TLS 1.2 for this process.
    try { [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12 } catch { }
    $elevated = Test-Elevated
    if ($elevated -and -not $DryRun) {
        Stop-Ojak 1 'not-installed' 'Run this script as the ordinary user, not Administrator: the installer is a per-user install and an elevated run would target the wrong account. Nothing was installed. (-DryRun and -Help work elevated.)'
    }

    Write-Ojak ('Resolving the release once from ' + $script:Repo + '...')
    $release = Resolve-Release $requested
    $sumsText = Invoke-WebText -Uri $release.Sums.Url -Api $false -What 'SHA256SUMS'
    if ($sumsText.Length -gt $script:MaxSumsBytes) { Stop-Ojak 1 'not-installed' 'SHA256SUMS is unexpectedly large. Nothing was installed.' }
    $expected = Get-ExpectedHash $sumsText $release.Installer.Name
    if ($null -ne $release.Installer.Digest -and $release.Installer.Digest -cne $expected) {
        Stop-Ojak 1 'not-installed' 'The GitHub asset digest and SHA256SUMS disagree for the installer. Refusing. Nothing was installed.'
    }
    $target = [version]$release.Version

    $state = Get-InstalledState
    if ($null -ne $state.Version -and $state.Version -gt $target) {
        Stop-Ojak 1 'not-installed' ('Ojak ' + $state.Version + ' is already installed and is newer than ' + $release.Version + '. Refusing to downgrade. Nothing was installed.')
    }
    $skipInstaller = $false
    if ($null -ne $state.Version -and $state.Version -eq $target) { $skipInstaller = $true }
    # NSIS: /S = silent. /UPDATE is what Tauri's own updater adds for an existing install
    # (skips the reinstall page, WebView2 and shortcut re-creation). A fresh install must not use it.
    $installerArgs = '/S'
    if ($state.Present) { $installerArgs = '/S /UPDATE' }

    Write-Disclaimers ([bool]$WithOmp)
    Write-Ojak ('Release ' + $release.Tag + ' | installer ' + $release.Installer.Name + ' | expected SHA-256 ' + $expected)
    Write-Ojak ('Install folder (current user): ' + $state.Dir)

    if ($DryRun) {
        Write-Ojak 'DRY RUN: plan only. No installer is downloaded or run, nothing is written, no service is changed.'
        if ($elevated) { Write-Ojak 'Note: this shell is elevated. A real install would be refused; use an ordinary-user shell.' }
        if ($skipInstaller) {
            Write-Ojak ('Plan 1/3: Ojak ' + $release.Version + ' is registered already; the installer would be skipped if "' + $state.Aam + ' --version" confirms it.')
        } else {
            Write-Ojak ('Plan 1/3: download ' + $release.Installer.Url + ', verify size and SHA-256, then run it silently (' + $installerArgs + '). The installer keeps its own safety checks and stops on active or uncertain sessions.')
            $blockers = @(Get-OjakProcesses $state.Dir)
            if ($blockers.Count -gt 0) {
                Write-Ojak ('Would block: Ojak files are in use by ' + (Format-Blockers $blockers) + '. A real run asks Ojak to close and never force-stops anything.')
            }
        }
        $setupText = 'aam setup'
        if ($WithOmp) { $setupText = 'aam setup --with-omp' }
        Write-Ojak ('Plan 2/3: run "' + $state.Aam + '" ' + $setupText.Substring(4))
        Write-Ojak ('Plan 3/3: run "' + $state.Aam + '" setup --check and require ready=true, serviceVersionMismatch=false, verified tools' + $(if ($WithOmp) { ', and omp broker/bridge/observer connected with error=null' } else { '' }))
        Write-Result 'dry-run' ''
        return 0
    }

    if (-not $skipInstaller) {
        Request-OjakRelease $state.Dir
        $script:TempDir = Join-Path ([IO.Path]::GetTempPath()) ('ojak-install-' + [guid]::NewGuid().ToString('N'))
        [void][IO.Directory]::CreateDirectory($script:TempDir)
        $file = Join-Path $script:TempDir $release.Installer.Name
        Write-Ojak ('Downloading ' + $release.Installer.Name + '...')
        try {
            Invoke-WebRequest -UseBasicParsing -Uri $release.Installer.Url -OutFile $file -UserAgent $script:UserAgent -TimeoutSec 900
        } catch {
            Stop-Ojak 1 'not-installed' 'Could not download the installer. Nothing was installed.'
        }
        if (-not (Test-Path -LiteralPath $file)) { Stop-Ojak 1 'not-installed' 'The installer download produced no file. Nothing was installed.' }
        $actualSize = (Get-Item -LiteralPath $file).Length
        if ($actualSize -ne $release.Installer.Size) {
            Stop-Ojak 1 'not-installed' ('Installer size ' + $actualSize + ' does not match the release asset size ' + $release.Installer.Size + '. It was not run. Nothing was installed.')
        }
        $actual = (Get-FileHash -LiteralPath $file -Algorithm SHA256).Hash.ToLowerInvariant()
        if ($actual -cne $expected) {
            Stop-Ojak 1 'not-installed' ('SHA-256 mismatch for ' + $release.Installer.Name + ': expected ' + $expected + ', got ' + $actual + '. The download was NOT run. Nothing was installed.')
        }
        Write-Ojak 'SHA-256 matches SHA256SUMS.'
        try {
            $sig = Get-AuthenticodeSignature -LiteralPath $file
            Write-Ojak ('Authenticode status: ' + [string]$sig.Status + ' (informational; the installer is not code-signed).')
        } catch { }

        $logPath = Join-Path ([IO.Path]::GetTempPath()) 'Ojak-installer.log'
        $logBefore = 0
        if (Test-Path -LiteralPath $logPath) { $logBefore = (Get-Item -LiteralPath $logPath).Length }
        Write-Ojak ('Running the installer silently (' + $installerArgs + ')...')
        $run = Invoke-External -File $file -Arguments $installerArgs -TimeoutSec 900 -Capture $false
        if (-not $run.Started) {
            Stop-Ojak 2 'installer-failed' ('The installer could not be started (' + $run.Stderr + '). Windows or antivirus may have blocked it; review that prompt or detection yourself. This script does not bypass it. Nothing was installed.')
        }
        if ($run.TimedOut) {
            Stop-Ojak 2 'installer-failed' ('The installer is still running after 15 minutes (PID ' + $run.Pid + '). It was not stopped. Check for a dialog, let it finish, then run this script again.')
        }
        if ($run.ExitCode -ne 0) {
            $note = ''
            if (Test-Path -LiteralPath $logPath) {
                $log = Get-Item -LiteralPath $logPath
                if ($log.Length -gt $logBefore) {
                    $lines = @(Get-Content -LiteralPath $logPath -Encoding UTF8 -Tail 5 | ForEach-Object { Protect-Text $_ 300 })
                    $note = ' Installer log (' + $logPath + '): ' + ($lines -join ' | ')
                }
            }
            Stop-Ojak 2 'installer-failed' ('The installer exited with code ' + $run.ExitCode + '. When its safety check stops, existing Ojak files are not replaced. Finish managed work, quit Orca and omp normally, then run this script again. If the details mention "installer recover", close Orca/omp and re-run the installer instead of editing files.' + $note)
        }
        Write-Ojak 'The installer reported success.'
        $state = Get-InstalledState
        $installedVer = Get-AamVersion $state.Aam
        $locationOk = $null -ne $state.Location -and [string]::Equals($state.Location, $state.Dir, [StringComparison]::OrdinalIgnoreCase)
        if ($null -eq $state.Version -or $state.Version -ne $target -or -not $locationOk -or $installedVer -cne $release.Version) {
            Write-Ojak ('Installer exit code 0 is not proof: registry version=' + $state.Version + ', location matches=' + $locationOk + ', "aam --version" reported ' + $installedVer + ' (expected ' + $release.Version + ').')
            Write-Result 'installed-unverified' 'Do not trust this install yet. Close Orca/omp, run this script again, and if it repeats, report the issue without editing files.'
            return 3
        }
    } else {
        $installedVer = Get-AamVersion $state.Aam
        if ($installedVer -cne $release.Version) {
            Write-Ojak ('Ojak ' + $release.Version + ' is registered but "' + $state.Aam + ' --version" reported ' + $installedVer + '. Quit Ojak/Orca/omp and run the installer again.')
            Write-Result 'installed-unverified' 'Close Orca/omp and re-run this script (a repair needs the registry entry removed by the normal uninstaller first).'
            return 3
        }
        Write-Ojak ('Ojak ' + $release.Version + ' is already installed at ' + $state.Dir + '; the installer was skipped.')
    }

    $aam = $state.Aam
    Write-Ojak ('Installed: Ojak ' + $release.Version + ' at ' + $state.Dir)

    $setupArgs = 'setup'
    if ($WithOmp) { $setupArgs = 'setup --with-omp' }
    Write-Ojak ('Running "' + $aam + '" ' + $setupArgs + ' ...')
    $setup = Invoke-External -File $aam -Arguments $setupArgs -TimeoutSec 240 -Capture $true
    $setupProblem = $null
    $setupJson = $null
    if (-not $setup.Started) {
        $setupProblem = 'aam setup could not be started: ' + $setup.Stderr
    } elseif ($setup.TimedOut) {
        $setupProblem = 'aam setup is still running after 4 minutes (PID ' + $setup.Pid + '); it was not stopped'
    } elseif ($setup.ExitCode -ne 0) {
        $code = Get-ErrorCode $setup.Stderr.Trim()
        $setupProblem = 'aam setup failed with exit code ' + $setup.ExitCode
        if ($code) { $setupProblem += ' (' + $code + ')' }
        $setupProblem += ': ' + (Protect-Text $setup.Stderr.Trim() 300)
        if ($code -eq 'AUTH_OVERRIDE_CONFLICT') {
            $setupProblem += ' Ojak found an API key, base URL, or auth helper setting that conflicts with subscription login. Review that setting yourself; this script never changes it.'
        }
    } else {
        $setupJson = Read-SetupJson $setup 'setup'
        if ($null -eq $setupJson) { $setupProblem = 'aam setup did not return a JSON object' }
    }
    if ($setupProblem) { Write-Ojak ('Setup problem: ' + $setupProblem) }

    Write-Ojak ('Running "' + $aam + '" setup --check ...')
    $checkRun = Invoke-External -File $aam -Arguments 'setup --check' -TimeoutSec 180 -Capture $true
    $check = Read-SetupJson $checkRun 'check'
    $problems = New-Object System.Collections.Generic.List[string]
    $warnings = @()
    $needsLogin = $false
    $checkOmp = $null
    if ($null -eq $check) {
        if (-not $checkRun.Started) { $problems.Add('aam setup --check could not be started') }
        elseif ($checkRun.TimedOut) { $problems.Add('aam setup --check is still running (PID ' + $checkRun.Pid + '); not stopped') }
        elseif ($checkRun.ExitCode -ne 0) { $problems.Add('aam setup --check failed with exit code ' + $checkRun.ExitCode + ': ' + (Protect-Text $checkRun.Stderr.Trim() 300)) }
        else { $problems.Add('aam setup --check did not return a JSON object') }
    } else {
        $eval = Test-CheckResult $check
        foreach ($p in $eval.Problems) { $problems.Add($p) }
        $warnings = $eval.Warnings
        $needsLogin = $eval.NeedsLogin
        $checkOmp = Get-Prop $check 'omp'
        foreach ($tool in $eval.Tools) {
            $tn = Protect-Text ([string](Get-Prop $tool 'tool')) 20
            if ((Get-Prop $tool 'account') -eq $true -and (Get-Prop $tool 'verified') -eq $true) { Write-Ojak ('  ' + $tn + ': account ready, command verified') }
            elseif ((Get-Prop $tool 'account') -eq $true) { Write-Ojak ('  ' + $tn + ': account found, NOT verified') }
            else { Write-Ojak ('  ' + $tn + ': no usable account') }
        }
    }
    if ($setupProblem) { $problems.Insert(0, $setupProblem) }
    foreach ($w in $warnings) { Write-Ojak ('Warning: ' + $w) }

    # `aam setup --check` reports omp status only; the connect result comes from `setup --with-omp`.
    $ompState = Get-OmpState $checkOmp
    if ($WithOmp) {
        Write-Ojak ('omp: ' + $ompState.Detail)
        $connectError = Get-Prop (Get-Prop $setupJson 'omp') 'error'
        if ($null -ne $connectError) { $ompState.Connected = $false }
        if (-not $ompState.Connected -and $null -ne $connectError) { Write-Ojak ('omp connect reported: ' + (Protect-Text ([string]$connectError) 300)) }
    } elseif ($ompState.Present) {
        Write-Ojak ('omp: detected but not requested, so it was not connected (' + $ompState.Detail + '). Re-run with -WithOmp only if you want it.')
    } else {
        Write-Ojak 'omp: not requested.'
    }

    if ($problems.Count -gt 0) {
        foreach ($p in $problems) { Write-Ojak ('Not ready: ' + $p) }
        $next = 'Fix the item above, then run: "' + $aam + '" setup --check (do not poll "setup --status"; it is always ready=false).'
        if ($needsLogin) {
            $next = 'No usable Claude Code or Codex account was found. Install/sign in with the official CLI yourself (this installer does not install them or take credentials), then run: "' + $aam + '" setup --check. ' + $next
        }
        Write-Ojak 'Ojak is installed, but setup is incomplete.'
        Write-Result 'installed-setup-incomplete' $next
        return 3
    }
    if ($WithOmp -and -not $ompState.Connected) {
        Write-Ojak 'Ojak is installed and ready, but the requested omp integration is not fully connected.'
        $next = 'Make sure the official omp CLI is installed, then run: "' + $aam + '" setup --with-omp'
        Write-Result 'installed-omp-incomplete' $next
        return 4
    }
    Write-Ojak 'Ojak is installed and ready. Existing terminals and sessions keep the old PATH; open a new terminal.'
    if ($WithOmp) { Write-Ojak 'omp: start a NEW omp session and choose an ojak-* model. A configuration check does not prove a paid model request works.' }
    Write-Result 'ready' ''
    return 0
}

$exitCode = 1
try {
    $mainOutput = @(Invoke-OjakMain)
    $exitCode = [int]$mainOutput[$mainOutput.Count - 1]
} catch {
    $data = $_.Exception.Data
    if ($data.Contains('OjakExitCode')) {
        Write-Ojak ([regex]::Replace($_.Exception.Message, '[\x00-\x08\x0B\x0C\x0E-\x1F]', ''))
        $exitCode = [int]$data['OjakExitCode']
        Write-Host ('RESULT: ' + [string]$data['OjakStatus'])
    } else {
        Write-Ojak ('Unexpected error: ' + (Protect-Text $_.Exception.Message 400))
        Write-Host 'RESULT: error'
        $exitCode = 1
    }
} finally {
    if ($script:TempDir -and (Test-Path -LiteralPath $script:TempDir)) {
        try { Remove-Item -LiteralPath $script:TempDir -Recurse -Force -ErrorAction Stop } catch { }
    }
}
exit $exitCode
