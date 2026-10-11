<#
Behavioral tests for install.ps1 (Windows only; Windows PowerShell 5.1 or PowerShell 7).

Run from a normal, non-elevated or elevated shell (elevation is simulated per case):
  powershell -NoProfile -File tests\install-ps1.tests.ps1
Optional: -Filter <substring of a case name>.

What is REAL in every case: the actual install.ps1 file runs in a fresh child
PowerShell session through its real parameter binding and real main flow; real
Process.Start of native executables; real SHA-256 hashing; real file writes.
What is FIXTURE (see install-ps1.harness.ps1): the GitHub network, the HKCU
registry reads, the process table, and the elevation check. The "installer" and
"aam.exe" are small native executables compiled below, so the production
installer is never run and nothing outside this test's temp folder is touched.
The test never installs Ojak, never contacts GitHub, and never edits the machine.
#>
[CmdletBinding()]
param([string]$Filter = '')

Set-StrictMode -Version 1.0
$ErrorActionPreference = 'Stop'

if ([Environment]::OSVersion.Platform -ne [PlatformID]::Win32NT) {
    Write-Host 'SKIPPED: install.ps1 behavioral tests need native Windows.'
    exit 0
}

$repoRoot = Split-Path -Parent $PSScriptRoot
$scriptPath = Join-Path $repoRoot 'install.ps1'
$harnessPath = Join-Path $PSScriptRoot 'install-ps1.harness.ps1'
$hostExe = (Get-Process -Id $PID).Path
$root = Join-Path ([IO.Path]::GetTempPath()) ('ojak-install-tests-' + [guid]::NewGuid().ToString('N'))
[void][IO.Directory]::CreateDirectory($root)

$script:Failures = New-Object System.Collections.Generic.List[string]
$script:Passed = 0
$script:CurrentCase = ''

function Assert-That {
    param([bool]$Condition, [string]$Message)
    if ($Condition) { $script:Passed++ } else { $script:Failures.Add($script:CurrentCase + ': ' + $Message) }
}

# ------------------------------------------------------------ fake native programs

$fakeSource = @'
using System;
using System.Collections.Generic;
using System.IO;
using System.Text;

static class Fake {
    static Dictionary<string, string> Cfg(string fx, string file) {
        var d = new Dictionary<string, string>();
        string path = Path.Combine(fx, file);
        if (!File.Exists(path)) return d;
        foreach (string line in File.ReadAllLines(path)) {
            int i = line.IndexOf('=');
            if (i > 0) d[line.Substring(0, i)] = line.Substring(i + 1);
        }
        return d;
    }
    static string Get(Dictionary<string, string> d, string key, string fallback) {
        string v; return d.TryGetValue(key, out v) ? v : fallback;
    }
    static void Out(string text) {
        byte[] b = new UTF8Encoding(false).GetBytes(text);
        Stream s = Console.OpenStandardOutput(); s.Write(b, 0, b.Length); s.Flush();
    }
    static void Err(string text) {
        byte[] b = new UTF8Encoding(false).GetBytes(text);
        Stream s = Console.OpenStandardError(); s.Write(b, 0, b.Length); s.Flush();
    }
    static string Read(string fx, string file) {
        string path = Path.Combine(fx, file);
        return File.Exists(path) ? File.ReadAllText(path, Encoding.UTF8) : "";
    }

    static int Main(string[] a) {
        string fx = Environment.GetEnvironmentVariable("OJAK_FX_DIR");
        string self = System.Reflection.Assembly.GetExecutingAssembly().Location;
        string args = string.Join(" ", a);
        if (Path.GetFileName(self).ToLowerInvariant().EndsWith("-setup.exe")) {
            File.AppendAllText(Path.Combine(fx, "installer-args.log"), args + "\r\n");
            var c = Cfg(fx, "installer.txt");
            int code = int.Parse(Get(c, "exit", "0"));
            if (code != 0) {
                string log = Path.Combine(Path.GetTempPath(), "Ojak-installer.log");
                File.AppendAllText(log, Get(c, "log", "fake installer failed") + "\r\n");
                return code;
            }
            string dir = Get(c, "install", "");
            Directory.CreateDirectory(dir);
            File.Copy(Get(c, "aamsrc", ""), Path.Combine(dir, "aam.exe"), true);
            if (Get(c, "register", "1") == "1")
                File.WriteAllText(Path.Combine(fx, "registry.txt"), Get(c, "version", "0.5.8") + "\r\n" + dir);
            return 0;
        }
        File.AppendAllText(Path.Combine(fx, "aam-calls.log"), self + "|" + args + "\r\n");
        var cfg = Cfg(fx, "aam.txt");
        if (args == "--version") { Out("aam " + Get(cfg, "version", "0.5.8") + "\r\n"); return 0; }
        string which = args == "setup --check" ? "check" : (args == "setup" || args == "setup --with-omp") ? "setup" : null;
        if (which == null) { Err("aam: unexpected arguments (BAD_ARGS)\r\n"); return 64; }
        Out(Read(fx, which + ".out"));
        Err(Read(fx, which + ".err"));
        return int.Parse(Get(cfg, which + "_exit", "0"));
    }
}
'@

$csc = Join-Path $env:WINDIR 'Microsoft.NET\Framework64\v4.0.30319\csc.exe'
if (-not (Test-Path -LiteralPath $csc)) { throw 'csc.exe (.NET Framework 4) was not found; it ships with Windows.' }
$fakeDir = Join-Path $root 'fake'
[void][IO.Directory]::CreateDirectory($fakeDir)
$fakeCs = Join-Path $fakeDir 'fake.cs'
$fakeExe = Join-Path $fakeDir 'fake.exe'
[IO.File]::WriteAllText($fakeCs, $fakeSource, (New-Object System.Text.UTF8Encoding($false)))
$compile = & $csc /nologo /target:exe ('/out:' + $fakeExe) $fakeCs 2>&1
if (-not (Test-Path -LiteralPath $fakeExe)) { throw ('Could not compile the fixture program: ' + ($compile -join ' ')) }

# ------------------------------------------------------------ fixtures

$apiLatest = 'https://api.github.com/repos/lapalai/ojak/releases/latest'
function Get-Url { param([string]$Tag, [string]$Name) return 'https://github.com/lapalai/ojak/releases/download/' + $Tag + '/' + $Name }

function New-Check {
    return [ordered]@{
        service = $true; shell = $true
        tools = @(
            [ordered]@{ tool = 'claude'; account = $true; connected = $true; verified = $true; verificationError = $null },
            [ordered]@{ tool = 'codex'; account = $false; connected = $false; verified = $null; verificationError = $null }
        )
        configured = $true; ompDetected = $false; ompSupported = $true; ready = $true
        notices = @(); serviceVersionMismatch = $false
    }
}

function Join-Json { param($Value) return (ConvertTo-Json -InputObject $Value -Depth 10 -Compress) }

# Runs one case. $Setup receives the case context hashtable $c and may change anything.
function Invoke-Case {
    param([string]$Name, [scriptblock]$Setup, [scriptblock]$Verify)
    if ($Filter -and $Name.IndexOf($Filter, [StringComparison]::OrdinalIgnoreCase) -lt 0) { return }
    $script:CurrentCase = $Name
    $dir = Join-Path $root ('case-' + [guid]::NewGuid().ToString('N'))
    $fx = Join-Path $dir 'fx'
    $localAppData = Join-Path $dir 'LocalAppData'
    $install = Join-Path $localAppData 'Ojak'
    foreach ($d in @($fx, (Join-Path $fx 'tmp'), $localAppData)) { [void][IO.Directory]::CreateDirectory($d) }

    $c = @{
        version = '0.5.8'; requestedVersion = $null; route = 'latest'
        elevated = $false; args = @{}; env = @{}
        processes = @(); closeRemoves = $false
        preinstalled = $null
        installer = @{ exit = '0'; register = '1'; version = '0.5.8'; log = 'fake installer failed' }
        aam = @{ version = '0.5.8'; setup_exit = '0'; check_exit = '0' }
        setupOut = $null; setupErr = ''; checkOut = $null; checkErr = ''
        check = (New-Check); setup = (New-Check)
        apiText = $null; mutateRelease = $null; sumsText = $null
        payload = $null; mutateInstallerBytes = $null
    }
    & $Setup $c
    $v = $c.version
    $tag = 'v' + $v
    $setupName = 'Ojak_' + $v + '_x64-setup.exe'

    # fixture "installer" and "aam.exe"
    $dl = Join-Path $dir 'download'
    [void][IO.Directory]::CreateDirectory($dl)
    $setupFile = Join-Path $dl $setupName
    Copy-Item -LiteralPath $fakeExe -Destination $setupFile
    # payload: replace the installer BEFORE hashing, so SHA256SUMS and the release agree with it.
    if ($c.payload) { [IO.File]::WriteAllBytes($setupFile, [byte[]]$c.payload) }
    $hash = (Get-FileHash -LiteralPath $setupFile -Algorithm SHA256).Hash.ToLowerInvariant()
    $size = (Get-Item -LiteralPath $setupFile).Length
    $aamTemplate = Join-Path $dir 'aam-template'
    [void][IO.Directory]::CreateDirectory($aamTemplate)
    Copy-Item -LiteralPath $fakeExe -Destination (Join-Path $aamTemplate 'aam.exe')

    $served = $setupFile
    if ($c.mutateInstallerBytes) {
        $served = Join-Path $dl 'served.exe'
        $bytes = [IO.File]::ReadAllBytes($setupFile)
        $bytes = & $c.mutateInstallerBytes $bytes
        [IO.File]::WriteAllBytes($served, $bytes)
    }

    $sums = $hash + '  ' + $setupName + "`n" + ('a' * 64) + '  Ojak_' + $v + "_aarch64.dmg`n"
    if ($null -ne $c.sumsText) { $sums = $c.sumsText.Replace('{HASH}', $hash).Replace('{NAME}', $setupName) }
    $sumsFile = Join-Path $dl 'SHA256SUMS'
    [IO.File]::WriteAllText($sumsFile, $sums, (New-Object System.Text.UTF8Encoding($false)))

    $release = [ordered]@{
        tag_name = $tag; draft = $false; prerelease = $false
        assets = @(
            [ordered]@{ name = 'latest.json'; size = 5248; browser_download_url = (Get-Url $tag 'latest.json'); state = 'uploaded'; digest = ('sha256:' + ('b' * 64)) },
            [ordered]@{ name = $setupName; size = $size; browser_download_url = (Get-Url $tag $setupName); state = 'uploaded'; digest = ('sha256:' + $hash) },
            [ordered]@{ name = 'SHA256SUMS'; size = 443; browser_download_url = (Get-Url $tag 'SHA256SUMS'); state = 'uploaded'; digest = ('sha256:' + ('c' * 64)) }
        )
    }
    if ($c.mutateRelease) { $release = & $c.mutateRelease $release }
    $apiFile = Join-Path $dl 'release.json'
    $apiText = Join-Json $release
    if ($null -ne $c.apiText) { $apiText = $c.apiText }
    [IO.File]::WriteAllText($apiFile, $apiText, (New-Object System.Text.UTF8Encoding($false)))

    $routes = [ordered]@{}
    if ($c.requestedVersion) { $routes['https://api.github.com/repos/lapalai/ojak/releases/tags/v' + $c.requestedVersion] = $apiFile }
    else { $routes[$apiLatest] = $apiFile }
    $routes[(Get-Url $tag 'SHA256SUMS')] = $sumsFile
    $routes[(Get-Url $tag $setupName)] = $served

    # installer / aam behaviour files read by the native fixtures
    $installerCfg = $c.installer.Clone()
    $installerCfg['install'] = $install
    $installerCfg['aamsrc'] = (Join-Path $aamTemplate 'aam.exe')
    [IO.File]::WriteAllLines((Join-Path $fx 'installer.txt'), @($installerCfg.GetEnumerator() | ForEach-Object { $_.Key + '=' + $_.Value }))
    [IO.File]::WriteAllLines((Join-Path $fx 'aam.txt'), @($c.aam.GetEnumerator() | ForEach-Object { $_.Key + '=' + $_.Value }))
    $setupOut = $c.setupOut; if ($null -eq $setupOut) { $setupOut = Join-Json $c.setup }
    $checkOut = $c.checkOut; if ($null -eq $checkOut) { $checkOut = Join-Json $c.check }
    [IO.File]::WriteAllText((Join-Path $fx 'setup.out'), $setupOut, (New-Object System.Text.UTF8Encoding($false)))
    [IO.File]::WriteAllText((Join-Path $fx 'setup.err'), $c.setupErr, (New-Object System.Text.UTF8Encoding($false)))
    [IO.File]::WriteAllText((Join-Path $fx 'check.out'), $checkOut, (New-Object System.Text.UTF8Encoding($false)))
    [IO.File]::WriteAllText((Join-Path $fx 'check.err'), $c.checkErr, (New-Object System.Text.UTF8Encoding($false)))

    if ($c.preinstalled) {
        [void][IO.Directory]::CreateDirectory($install)
        Copy-Item -LiteralPath (Join-Path $aamTemplate 'aam.exe') -Destination (Join-Path $install 'aam.exe')
        [IO.File]::WriteAllText((Join-Path $fx 'registry.txt'), $c.preinstalled + "`r`n" + $install)
    }

    $envMap = [ordered]@{}
    foreach ($key in $c.env.Keys) { $envMap[$key] = $c.env[$key] }
    $scenario = [ordered]@{
        scriptPath = $scriptPath; fxDir = $fx; localAppData = $localAppData
        elevated = $c.elevated; args = $c.args; env = $envMap; routes = $routes
        processes = @($c.processes); closeRemoves = $c.closeRemoves
    }
    $scenarioPath = Join-Path $dir 'scenario.json'
    [IO.File]::WriteAllText($scenarioPath, (Join-Json $scenario), (New-Object System.Text.UTF8Encoding($false)))

    # run the real script in a child session
    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = $hostExe
    $psi.Arguments = '-NoProfile -NonInteractive -File "' + $harnessPath + '" -ScenarioPath "' + $scenarioPath + '"'
    $psi.UseShellExecute = $false
    $psi.CreateNoWindow = $true
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError = $true
    $proc = [System.Diagnostics.Process]::Start($psi)
    $outTask = $proc.StandardOutput.ReadToEndAsync()
    $errTask = $proc.StandardError.ReadToEndAsync()
    if (-not $proc.WaitForExit(240000)) {
        $proc.Kill()
        $script:Failures.Add($Name + ': timed out after 240 s')
        return
    }
    $proc.WaitForExit()
    $text = $outTask.Result + "`n" + $errTask.Result

    $ctx = [pscustomobject]@{
        Exit = $proc.ExitCode; Out = $text; Fx = $fx; Install = $install; LocalAppData = $localAppData
        Tag = $tag; SetupName = $setupName
    }
    & $Verify $ctx
}

function Read-Log {
    param($Ctx, [string]$File)
    $p = Join-Path $Ctx.Fx $File
    if (-not (Test-Path -LiteralPath $p)) { return @() }
    return @(Get-Content -LiteralPath $p -Encoding UTF8 | Where-Object { $_ })
}
function Assert-Exit { param($Ctx, [int]$Code) Assert-That ($Ctx.Exit -eq $Code) ('expected exit ' + $Code + ' but got ' + $Ctx.Exit + "`n--- output ---`n" + $Ctx.Out + '--------------') }
function Assert-Says { param($Ctx, [string]$Text) Assert-That ($Ctx.Out.Contains($Text)) ('output should contain "' + $Text + '"' + "`n--- output ---`n" + $Ctx.Out + '--------------') }
function Assert-NotSays { param($Ctx, [string]$Text) Assert-That (-not $Ctx.Out.Contains($Text)) ('output must not contain "' + $Text + '"') }
function Assert-InstallerNotRun { param($Ctx) Assert-That (@(Read-Log $Ctx 'installer-args.log').Count -eq 0) 'the installer must not have been run' }
function Assert-NothingInstalled {
    param($Ctx)
    Assert-InstallerNotRun $Ctx
    Assert-That (-not (Test-Path -LiteralPath (Join-Path $Ctx.Install 'aam.exe'))) 'no aam.exe may have been installed'
    Assert-That (@(Read-Log $Ctx 'aam-calls.log').Count -eq 0) 'aam must not have been run'
    Assert-That (@(Get-ChildItem -LiteralPath (Join-Path $Ctx.Fx 'tmp') -Filter 'ojak-install-*' -ErrorAction SilentlyContinue).Count -eq 0) 'the download folder must be cleaned up'
}
function Assert-NoForceKill { param($Ctx) Assert-That (-not (@(Read-Log $Ctx 'calls.log') -match 'FORCE-KILL')) 'nothing may be force-stopped' }
function Test-SamePath { param([string]$A, [string]$B) return [string]::Equals($A.TrimEnd('\'), $B.TrimEnd('\'), [StringComparison]::OrdinalIgnoreCase) }

# ------------------------------------------------------------ cases

# -- the file itself (cheap guards that real 5.1 behavior depends on)
$script:CurrentCase = 'script-file'
if (-not $Filter -or 'script-file'.IndexOf($Filter, [StringComparison]::OrdinalIgnoreCase) -ge 0) {
    $bytes = [IO.File]::ReadAllBytes($scriptPath)
    Assert-That (-not ($bytes | Where-Object { $_ -gt 127 })) 'install.ps1 must be ASCII (Windows PowerShell 5.1 reads BOM-less files as ANSI)'
    $errors = $null; $tokens = $null
    [void][System.Management.Automation.Language.Parser]::ParseFile($scriptPath, [ref]$tokens, [ref]$errors)
    Assert-That ($errors.Count -eq 0) ('install.ps1 must parse on this PowerShell: ' + (($errors | ForEach-Object { $_.Message }) -join '; '))
}

Invoke-Case 'help prints usage and touches nothing' { param($c) $c.args = @{ Help = $true } } {
    param($r)
    Assert-Exit $r 0
    Assert-Says $r 'Usage: .\install.ps1'
    Assert-Says $r '-WithOmp'
    Assert-That (@(Read-Log $r 'web.log').Count -eq 0) '-Help must not use the network'
    Assert-NothingInstalled $r
}

Invoke-Case 'dry-run plans only and is read-only' { param($c) $c.args = @{ DryRun = $true } } {
    param($r)
    Assert-Exit $r 0
    Assert-Says $r 'DRY RUN'
    Assert-Says $r 'RESULT: dry-run'
    Assert-Says $r 'NOT code-signed'
    Assert-Says $r ($r.SetupName)
    $web = @(Read-Log $r 'web.log')
    Assert-That (-not ($web -match 'x64-setup.exe')) 'dry-run must not download the installer'
    Assert-That (@($web -match 'releases/latest').Count -eq 1) 'the release is resolved exactly once'
    Assert-That (-not (Test-Path -LiteralPath (Join-Path $r.Fx 'registry.txt'))) 'dry-run must not change registry state'
    Assert-That (-not (Test-Path -LiteralPath $r.Install)) 'dry-run must not create the install folder'
    Assert-NothingInstalled $r
}

Invoke-Case 'dry-run works when elevated (read-only)' { param($c) $c.args = @{ DryRun = $true }; $c.elevated = $true } {
    param($r)
    Assert-Exit $r 0
    Assert-Says $r 'RESULT: dry-run'
    Assert-Says $r 'this shell is elevated'
    Assert-NothingInstalled $r
}

Invoke-Case 'dry-run with -WithOmp states the omp plan and the risk' { param($c) $c.args = @{ DryRun = $true; WithOmp = $true } } {
    param($r)
    Assert-Exit $r 0
    Assert-Says $r 'setup --with-omp'
    Assert-Says $r 'may conflict with provider terms'
    Assert-NothingInstalled $r
}

Invoke-Case 'dry-run reports a file lock without touching anything' {
    param($c)
    $c.args = @{ DryRun = $true }
    $c.processes = @(@{ pid = 6001; parent = 1; name = 'ai-account-manager.exe'; path = '{INSTALL}\ai-account-manager.exe' })
} {
    param($r)
    Assert-Exit $r 0
    Assert-Says $r 'Would block'
    Assert-That (@(Read-Log $r 'calls.log').Count -eq 0) 'dry-run must not even ask the app to close'
    Assert-NothingInstalled $r
}

Invoke-Case 'real install refuses an elevated shell' { param($c) $c.elevated = $true } {
    param($r)
    Assert-Exit $r 1
    Assert-Says $r 'not Administrator'
    Assert-NothingInstalled $r
    Assert-That (-not (@(Read-Log $r 'web.log') -match 'x64-setup.exe')) 'must not download when elevated'
}

Invoke-Case 'fresh install: silent installer, absolute-path setup and check, ready' { param($c) } {
    param($r)
    Assert-Exit $r 0
    Assert-Says $r 'RESULT: ready'
    Assert-Says $r 'SHA-256 matches SHA256SUMS'
    Assert-Says $r 'NOT code-signed'
    $instArgs = @(Read-Log $r 'installer-args.log')
    Assert-That ($instArgs.Count -eq 1 -and $instArgs[0] -ceq '/S') ('fresh install must use exactly /S, got: ' + ($instArgs -join ' || '))
    $calls = @(Read-Log $r 'aam-calls.log')
    Assert-That ($calls.Count -eq 3) ('expected --version, setup, setup --check but got ' + $calls.Count)
    $expectedAam = Join-Path $r.Install 'aam.exe'
    $order = @('--version', 'setup', 'setup --check')
    for ($i = 0; $i -lt [Math]::Min(3, $calls.Count); $i++) {
        $parts = $calls[$i].Split('|')
        Assert-That (Test-SamePath $parts[0] $expectedAam) ('call ' + $i + ' must use the installed absolute path ' + $expectedAam + ', got ' + $parts[0])
        Assert-That ($parts[1] -ceq $order[$i]) ('call ' + $i + ' args should be "' + $order[$i] + '" but were "' + $parts[1] + '"')
    }
    Assert-That (@(Get-ChildItem -LiteralPath (Join-Path $r.Fx 'tmp') -Filter 'ojak-install-*' -ErrorAction SilentlyContinue).Count -eq 0) 'download folder removed'
    Assert-That ((@(Read-Log $r 'web.log') | Where-Object { $_ -match 'releases/latest' }).Count -eq 1) 'latest resolved exactly once'
}

Invoke-Case 'existing older install is updated with /S /UPDATE' { param($c) $c.preinstalled = '0.5.7' } {
    param($r)
    Assert-Exit $r 0
    $instArgs = @(Read-Log $r 'installer-args.log')
    Assert-That ($instArgs.Count -eq 1 -and $instArgs[0] -ceq '/S /UPDATE') ('update must use /S /UPDATE, got: ' + ($instArgs -join ' || '))
}

Invoke-Case 'same version already installed skips the installer but still sets up' { param($c) $c.preinstalled = '0.5.8' } {
    param($r)
    Assert-Exit $r 0
    Assert-InstallerNotRun $r
    Assert-That (-not (@(Read-Log $r 'web.log') -match 'x64-setup.exe')) 'no installer download when already installed'
    $calls = @(Read-Log $r 'aam-calls.log')
    Assert-That ($calls.Count -eq 3) 'version check, setup, check still run'
}

Invoke-Case 'newer installed version is never downgraded' { param($c) $c.preinstalled = '0.6.0' } {
    param($r)
    Assert-Exit $r 1
    Assert-Says $r 'Refusing to downgrade'
    Assert-InstallerNotRun $r
}

Invoke-Case '-Version resolves that tag exactly once' {
    param($c)
    $c.requestedVersion = '0.5.8'
    $c.args = @{ Version = 'v0.5.8' }
} {
    param($r)
    Assert-Exit $r 0
    $api = @(Read-Log $r 'web.log') | Where-Object { $_ -match 'api.github.com' }
    Assert-That (@($api).Count -eq 1 -and @($api)[0] -ceq 'https://api.github.com/repos/lapalai/ojak/releases/tags/v0.5.8') 'must read only the tag endpoint'
}

Invoke-Case '-Version rejects a different tag from GitHub' {
    param($c)
    $c.requestedVersion = '0.5.7'
    $c.args = @{ Version = '0.5.7' }
} {
    param($r)
    Assert-Exit $r 1
    Assert-Says $r 'for requested version 0.5.7'
    Assert-NothingInstalled $r
}

foreach ($bad in @('1.2', 'latest', '0.5.8-rc1', '../0.5.8', '0.5.8 ')) {
    $badValue = $bad
    Invoke-Case ('-Version rejects "' + $badValue + '" before any network call') { param($c) $c.args = @{ Version = $badValue } } {
        param($r)
        Assert-Exit $r 1
        Assert-Says $r '-Version must look like'
        Assert-That (@(Read-Log $r 'web.log').Count -eq 0) 'no network call'
        Assert-NothingInstalled $r
    }
}

Invoke-Case 'unsupported architecture is refused' {
    param($c)
    $c.env = @{ PROCESSOR_ARCHITECTURE = 'ARM64'; PROCESSOR_ARCHITEW6432 = $null }
} {
    param($r)
    Assert-Exit $r 1
    Assert-Says $r 'Windows x64'
    Assert-That (@(Read-Log $r 'web.log').Count -eq 0) 'no network call'
    Assert-NothingInstalled $r
}

# -- malformed manifests
$manifestCases = @(
    @{ n = 'release JSON is not JSON'; text = 'not json {'; says = 'not a valid JSON object' },
    @{ n = 'release JSON is an array'; text = '[]'; says = 'not a valid JSON object' },
    @{ n = 'release JSON is empty'; text = ''; says = 'not a valid JSON object' },
    @{ n = 'tag is not vX.Y.Z'; mut = { param($x) $x['tag_name'] = 'v0.5.8-rc1'; $x }; says = 'not vX.Y.Z' },
    @{ n = 'tag is missing'; mut = { param($x) $x.Remove('tag_name'); $x }; says = 'not vX.Y.Z' },
    @{ n = 'draft release'; mut = { param($x) $x['draft'] = $true; $x }; says = 'is a draft' },
    @{ n = 'draft flag is a string'; mut = { param($x) $x['draft'] = 'false'; $x }; says = 'is a draft' },
    @{ n = 'prerelease'; mut = { param($x) $x['prerelease'] = $true; $x }; says = 'is a prerelease' },
    @{ n = 'installer asset missing'; mut = { param($x) $x['assets'] = @($x['assets'] | Where-Object { $_['name'] -notlike '*x64-setup.exe' }); $x }; says = 'exactly one asset named Ojak_0.5.8_x64-setup.exe' },
    @{ n = 'SHA256SUMS asset missing'; mut = { param($x) $x['assets'] = @($x['assets'] | Where-Object { $_['name'] -ne 'SHA256SUMS' }); $x }; says = 'exactly one asset named SHA256SUMS' },
    @{ n = 'duplicate installer asset'; mut = { param($x) $x['assets'] = @($x['assets']) + @($x['assets'] | Where-Object { $_['name'] -like '*x64-setup.exe' }); $x }; says = 'found 2' },
    @{ n = 'installer URL on another host'; mut = { param($x) foreach ($a in $x['assets']) { if ($a['name'] -like '*x64-setup.exe') { $a['browser_download_url'] = 'https://evil.example/lapalai/ojak/releases/download/v0.5.8/Ojak_0.5.8_x64-setup.exe' } }; $x }; says = 'its URL is not exactly' },
    @{ n = 'installer URL from another repository'; mut = { param($x) foreach ($a in $x['assets']) { if ($a['name'] -like '*x64-setup.exe') { $a['browser_download_url'] = 'https://github.com/attacker/ojak/releases/download/v0.5.8/Ojak_0.5.8_x64-setup.exe' } }; $x }; says = 'its URL is not exactly' },
    @{ n = 'installer URL points at another tag'; mut = { param($x) foreach ($a in $x['assets']) { if ($a['name'] -like '*x64-setup.exe') { $a['browser_download_url'] = 'https://github.com/lapalai/ojak/releases/download/v0.5.7/Ojak_0.5.8_x64-setup.exe' } }; $x }; says = 'its URL is not exactly' },
    @{ n = 'installer URL carries a query string'; mut = { param($x) foreach ($a in $x['assets']) { if ($a['name'] -like '*x64-setup.exe') { $a['browser_download_url'] += '?x=1' } }; $x }; says = 'its URL is not exactly' },
    @{ n = 'SHA256SUMS URL on another host'; mut = { param($x) foreach ($a in $x['assets']) { if ($a['name'] -eq 'SHA256SUMS') { $a['browser_download_url'] = 'https://evil.example/SHA256SUMS' } }; $x }; says = 'its URL is not exactly' },
    @{ n = 'asset not in uploaded state'; mut = { param($x) foreach ($a in $x['assets']) { if ($a['name'] -like '*x64-setup.exe') { $a['state'] = 'open' } }; $x }; says = 'not in the uploaded state' },
    @{ n = 'asset size is a string'; mut = { param($x) foreach ($a in $x['assets']) { if ($a['name'] -like '*x64-setup.exe') { $a['size'] = '9614054' } }; $x }; says = 'missing or implausible size' },
    @{ n = 'asset size is negative'; mut = { param($x) foreach ($a in $x['assets']) { if ($a['name'] -like '*x64-setup.exe') { $a['size'] = -5 } }; $x }; says = 'missing or implausible size' },
    @{ n = 'asset size is absurdly large'; mut = { param($x) foreach ($a in $x['assets']) { if ($a['name'] -like '*x64-setup.exe') { $a['size'] = 99999999999 } }; $x }; says = 'missing or implausible size' },
    @{ n = 'asset digest is malformed'; mut = { param($x) foreach ($a in $x['assets']) { if ($a['name'] -like '*x64-setup.exe') { $a['digest'] = 'sha256:XYZ' } }; $x }; says = 'malformed digest' },
    @{ n = 'asset digest disagrees with SHA256SUMS'; mut = { param($x) foreach ($a in $x['assets']) { if ($a['name'] -like '*x64-setup.exe') { $a['digest'] = 'sha256:' + ('d' * 64) } }; $x }; says = 'digest and SHA256SUMS disagree' },
    @{ n = 'SHA256SUMS has a malformed line'; sums = "garbage line`n{HASH}  {NAME}`n"; says = 'SHA256SUMS is malformed' },
    @{ n = 'SHA256SUMS uses an uppercase hash'; sums = "{HASH}  {NAME}`n".Replace('{HASH}', ('A' * 64)); says = 'SHA256SUMS is malformed' },
    @{ n = 'SHA256SUMS does not list the installer'; sums = ('a' * 64) + "  Ojak_0.5.8_aarch64.dmg`n"; says = 'exactly once (found 0)' },
    @{ n = 'SHA256SUMS lists the installer twice'; sums = "{HASH}  {NAME}`n{HASH}  {NAME}`n"; says = 'exactly once (found 2)' },
    @{ n = 'SHA256SUMS names a path'; sums = "{HASH}  ../{NAME}`n"; says = 'SHA256SUMS is malformed' }
)
foreach ($case in $manifestCases) {
    $m = $case
    Invoke-Case ('malformed manifest: ' + $m.n) {
        param($c)
        if ($m.ContainsKey('text')) { $c.apiText = $m.text }
        if ($m.ContainsKey('mut')) { $c.mutateRelease = $m.mut }
        if ($m.ContainsKey('sums')) { $c.sumsText = $m.sums }
    } {
        param($r)
        Assert-Exit $r 1
        Assert-Says $r $m.says
        Assert-Says $r 'Nothing was installed'
        Assert-That (-not (@(Read-Log $r 'web.log') -match 'x64-setup.exe')) 'the installer must not be downloaded for a bad manifest'
        Assert-NothingInstalled $r
    }
}

Invoke-Case 'unreachable GitHub is a clean not-installed failure' {
    param($c)
    $c.args = @{ Version = '0.5.8' }
    $c.requestedVersion = '0.9.9'
} {
    param($r)
    Assert-Exit $r 1
    Assert-Says $r 'Could not read the release metadata'
    Assert-NothingInstalled $r
}

# -- hash / size checks on the downloaded bytes
Invoke-Case 'hash mismatch: tampered download is never run' {
    param($c)
    $c.mutateInstallerBytes = { param($b) $b[$b.Length - 1] = [byte](($b[$b.Length - 1] + 1) % 256); , $b }
} {
    param($r)
    Assert-Exit $r 1
    Assert-Says $r 'SHA-256 mismatch'
    Assert-Says $r 'was NOT run'
    Assert-NothingInstalled $r
}

Invoke-Case 'SHA256SUMS lists a different hash than the download' {
    param($c)
    $c.sumsText = ('f' * 64) + "  {NAME}`n"
    $c.mutateRelease = { param($x) foreach ($a in $x['assets']) { if ($a['name'] -like '*x64-setup.exe') { $a.Remove('digest') } }; $x }
} {
    param($r)
    Assert-Exit $r 1
    Assert-Says $r 'SHA-256 mismatch'
    Assert-NothingInstalled $r
}

Invoke-Case 'download size differs from the release asset size' {
    param($c)
    $c.mutateInstallerBytes = { param($b) , ($b + [byte[]](1, 2, 3)) }
} {
    param($r)
    Assert-Exit $r 1
    Assert-Says $r 'does not match the release asset size'
    Assert-NothingInstalled $r
}

# -- installer failures
Invoke-Case 'installer failure: its own safety stop leaves nothing replaced' {
    param($c)
    $c.preinstalled = '0.5.7'
    $c.installer = @{ exit = '1'; register = '1'; version = '0.5.8'; log = 'Could not safely stop the existing Ojak. No files were replaced.' }
} {
    param($r)
    Assert-Exit $r 2
    Assert-Says $r 'RESULT: installer-failed'
    Assert-Says $r 'exited with code 1'
    Assert-Says $r 'Could not safely stop the existing Ojak'
    Assert-Says $r 'installer recover'
    Assert-That (@(Read-Log $r 'aam-calls.log').Count -eq 0) 'setup must not run after an installer failure'
    $reg = @(Read-Log $r 'registry.txt')
    Assert-That ($reg.Count -gt 0 -and $reg[0] -ceq '0.5.7') 'existing install registration must be untouched'
    Assert-That (@(Get-ChildItem -LiteralPath (Join-Path $r.Fx 'tmp') -Filter 'ojak-install-*' -ErrorAction SilentlyContinue).Count -eq 0) 'download folder removed'
}

Invoke-Case 'installer that cannot start is reported without bypassing anything' {
    param($c)
    # Hash, size and release metadata all agree with this non-executable payload, so the
    # checks pass and Windows itself refuses to start it (as antivirus or policy might).
    $c.payload = [Text.Encoding]::ASCII.GetBytes('this is not an executable')
} {
    param($r)
    Assert-Exit $r 2
    Assert-Says $r 'RESULT: installer-failed'
    Assert-Says $r 'could not be started'
    Assert-Says $r 'does not bypass it'
    Assert-InstallerNotRun $r
    Assert-That (@(Read-Log $r 'aam-calls.log').Count -eq 0) 'setup must not run'
    Assert-That (-not (Test-Path -LiteralPath (Join-Path $r.Install 'aam.exe'))) 'nothing installed'
}

Invoke-Case 'installer exit 0 but registry missing is not trusted' {
    param($c)
    $c.installer = @{ exit = '0'; register = '0'; version = '0.5.8'; log = '' }
} {
    param($r)
    Assert-Exit $r 3
    Assert-Says $r 'RESULT: installed-unverified'
    Assert-Says $r 'exit code 0 is not proof'
    Assert-That (-not (@(Read-Log $r 'aam-calls.log') -match '\|setup')) 'setup must not run on an unverified install'
}

Invoke-Case 'installer exit 0 but installed aam reports another version' {
    param($c)
    $c.aam = @{ version = '0.0.1'; setup_exit = '0'; check_exit = '0' }
} {
    param($r)
    Assert-Exit $r 3
    Assert-Says $r 'RESULT: installed-unverified'
    Assert-Says $r 'reported 0.0.1'
}

# -- file locks / managed session
Invoke-Case 'running app is asked to close, never force-stopped; still locked -> stop' {
    param($c)
    $c.processes = @(
        @{ pid = 6001; parent = 1; name = 'ai-account-manager.exe'; path = '\\?\{INSTALL}\ai-account-manager.exe' },
        @{ pid = 6003; parent = 1; name = 'aam-service.exe'; path = '{INSTALL}\aam-service.exe' },
        @{ pid = 6004; parent = 1; name = 'Ojak.exe'; path = 'C:\Somewhere\Else\Ojak.exe' }
    )
    $c.closeRemoves = $false
} {
    param($r)
    Assert-Exit $r 1
    Assert-Says $r 'Ojak files are in use'
    Assert-Says $r 'ai-account-manager.exe (PID 6001'
    Assert-NotSays $r 'PID 6004'
    Assert-NotSays $r 'aam-service.exe (PID'
    Assert-That ((@(Read-Log $r 'calls.log') -contains 'CloseMainWindow pid=6001')) 'must ask the app window to close'
    Assert-NoForceKill $r
    Assert-NothingInstalled $r
}

Invoke-Case 'app closes when asked: install proceeds' {
    param($c)
    $c.processes = @(@{ pid = 6001; parent = 1; name = 'ai-account-manager.exe'; path = '{INSTALL}\ai-account-manager.exe'; goneAfterClose = $true })
    $c.closeRemoves = $true
} {
    param($r)
    Assert-Exit $r 0
    Assert-That ((@(Read-Log $r 'calls.log') -contains 'CloseMainWindow pid=6001')) 'must ask the app window to close'
    Assert-NoForceKill $r
    Assert-That (@(Read-Log $r 'installer-args.log').Count -eq 1) 'installer ran after the app closed'
}

Invoke-Case 'locked aam.exe (omp/Orca observer) is never terminated' {
    param($c)
    $c.processes = @(@{ pid = 6002; parent = 777; name = 'aam.exe'; path = '{INSTALL}\aam.exe' })
} {
    param($r)
    Assert-Exit $r 1
    Assert-Says $r 'Orca'
    Assert-Says $r 'aam.exe (PID 6002'
    Assert-That (@(Read-Log $r 'calls.log').Count -eq 0) 'no window to close and no kill'
    Assert-NothingInstalled $r
}

Invoke-Case 'script running inside an Ojak-managed session refuses to continue' {
    param($c)
    $c.processes = @(@{ asParent = $true; name = 'aam.exe'; path = '{INSTALL}\aam.exe' })
} {
    param($r)
    Assert-Exit $r 1
    Assert-Says $r 'running inside an Ojak-managed session'
    Assert-That (@(Read-Log $r 'calls.log').Count -eq 0) 'must not ask anything to close'
    Assert-NothingInstalled $r
}

# -- setup / check evaluation
Invoke-Case 'needs login: installed, no usable account' {
    param($c)
    $chk = New-Check
    $chk.tools = @(
        [ordered]@{ tool = 'claude'; account = $false; connected = $false; verified = $null; verificationError = $null },
        [ordered]@{ tool = 'codex'; account = $false; connected = $false; verified = $null; verificationError = $null }
    )
    $chk.configured = $false; $chk.ready = $false
    $c.check = $chk
} {
    param($r)
    Assert-Exit $r 3
    Assert-Says $r 'RESULT: installed-setup-incomplete'
    Assert-Says $r 'Ojak is installed, but setup is incomplete'
    Assert-Says $r 'No usable Claude Code or Codex account'
    Assert-Says $r 'ready is false'
    Assert-Says $r 'do not poll "setup --status"'
    Assert-NotSays $r 'Ojak is installed and ready'
}

Invoke-Case 'verification failure of an account keeps setup incomplete' {
    param($c)
    $chk = New-Check
    $chk.tools[0].verified = $false
    $chk.tools[0].verificationError = 'claude resolves to a different command'
    $chk.ready = $false
    $chk.notices = @([ordered]@{ step = 'terminal'; tool = 'claude'; code = 'windows-mismatch'; message = 'claude is not the managed shim' })
    $c.check = $chk
} {
    param($r)
    Assert-Exit $r 3
    Assert-Says $r 'claude: new-terminal command check did not pass'
    Assert-Says $r 'windows-mismatch'
}

Invoke-Case 'ready true but service version mismatch is not success' {
    param($c)
    $chk = New-Check
    $chk.serviceVersionMismatch = $true
    $chk.notices = @([ordered]@{ step = 'service-version'; tool = $null; code = 'service-version-mismatch'; message = 'service differs' })
    $c.check = $chk
} {
    param($r)
    Assert-Exit $r 3
    Assert-Says $r 'serviceVersionMismatch'
    Assert-Says $r 'installed, but setup is incomplete'
}

Invoke-Case 'ready true but blocking path-shadow notice is not success' {
    param($c)
    $chk = New-Check
    $chk.notices = @([ordered]@{ step = 'terminal'; tool = $null; code = 'path-shadow'; message = 'system PATH has another claude' })
    $c.check = $chk
} {
    param($r)
    Assert-Exit $r 3
    Assert-Says $r 'notice path-shadow'
}

Invoke-Case 'ready true with only an informational notice is success with a warning' {
    param($c)
    $chk = New-Check
    $chk.notices = @([ordered]@{ step = 'terminal'; tool = $null; code = 'step-warning'; message = 'Heads up: open a new terminal' })
    $c.check = $chk
} {
    param($r)
    Assert-Exit $r 0
    Assert-Says $r 'Warning: notice step-warning'
    Assert-Says $r 'RESULT: ready'
}

Invoke-Case 'malformed check JSON (missing fields) is never ready' { param($c) $c.checkOut = '{"ready":true}' } {
    param($r)
    Assert-Exit $r 3
    Assert-Says $r 'malformed result'
    Assert-NotSays $r 'RESULT: ready'
}

Invoke-Case 'check output that is not JSON is never ready' { param($c) $c.checkOut = 'Ojak is fine, trust me' } {
    param($r)
    Assert-Exit $r 3
    Assert-Says $r 'did not return a JSON object'
}

Invoke-Case 'setup output must be JSON even when a later check passes' { param($c) $c.setupOut = 'not JSON' } {
    param($r)
    Assert-Exit $r 3
    Assert-NotSays $r 'RESULT: ready'
}

Invoke-Case 'ready as a string is rejected' {
    param($c)
    $chk = New-Check
    $chk.ready = 'true'
    $c.check = $chk
} {
    param($r)
    Assert-Exit $r 3
    Assert-Says $r 'malformed result: "ready"'
}

Invoke-Case 'aam setup --check failing with a non-zero exit code' {
    param($c)
    $c.aam = @{ version = '0.5.8'; setup_exit = '0'; check_exit = '1' }
    $c.checkOut = ''
    $c.checkErr = 'aam: could not verify (CHECK_FAILED)'
} {
    param($r)
    Assert-Exit $r 3
    Assert-Says $r 'setup --check failed with exit code 1'
}

Invoke-Case 'AUTH_OVERRIDE_CONFLICT stops and is never auto-fixed' {
    param($c)
    $c.aam = @{ version = '0.5.8'; setup_exit = '1'; check_exit = '0' }
    $c.setupOut = ''
    $c.setupErr = 'aam: conflicting auth setting found (AUTH_OVERRIDE_CONFLICT)'
} {
    param($r)
    Assert-Exit $r 3
    Assert-Says $r 'AUTH_OVERRIDE_CONFLICT'
    Assert-Says $r 'this script never changes it'
    Assert-Says $r 'RESULT: installed-setup-incomplete'
    $subcommands = @(Read-Log $r 'aam-calls.log') | ForEach-Object { $_.Split('|')[1] }
    Assert-That (@($subcommands | Where-Object { $_ -notin @('--version', 'setup', 'setup --check') }).Count -eq 0) ('only version/setup/check may run, got: ' + ($subcommands -join ', '))
}

Invoke-Case 'setup failure output never leaks emails or tokens' {
    param($c)
    $c.aam = @{ version = '0.5.8'; setup_exit = '1'; check_exit = '0' }
    $c.setupOut = ''
    $c.setupErr = 'aam: failed for someone@example.com with sk-abcdef0123456789abcdef0123 and Bearer eyJhbGciOiJIUzI1NiJ9abcdef (SETUP_FAILED)'
} {
    param($r)
    Assert-Exit $r 3
    Assert-NotSays $r 'someone@example.com'
    Assert-NotSays $r 'sk-abcdef0123456789abcdef0123'
    Assert-NotSays $r 'eyJhbGciOiJIUzI1NiJ9abcdef'
    Assert-Says $r 'SETUP_FAILED'
}

# -- omp
function New-OmpResult {
    param([bool]$Broker, [bool]$Bridge, [bool]$Observer, $ErrorText)
    return [ordered]@{ broker = $Broker; bridge = $Bridge; observer = $Observer; error = $ErrorText; futureLaunchesOnly = $true }
}

Invoke-Case 'omp requested and fully connected' {
    param($c)
    $c.args = @{ WithOmp = $true }
    $c.check['omp'] = New-OmpResult $true $true $true $null
    $c.setup['omp'] = New-OmpResult $true $true $true $null
} {
    param($r)
    Assert-Exit $r 0
    Assert-Says $r 'RESULT: ready'
    Assert-Says $r 'broker=connected, bridge=connected, observer=connected'
    Assert-Says $r 'choose an ojak-* model'
    Assert-Says $r 'may conflict with provider terms'
    $calls = @(Read-Log $r 'aam-calls.log') | ForEach-Object { $_.Split('|')[1] }
    Assert-That ($calls -contains 'setup --with-omp') 'must run aam setup --with-omp'
    Assert-That (-not ($calls -contains 'setup')) 'plain setup must not run when -WithOmp is used'
    Assert-That ($calls -contains 'setup --check') 'must run the check'
}

Invoke-Case 'omp partial: bridge down and error set -> exit 4, base install ready' {
    param($c)
    $c.args = @{ WithOmp = $true }
    $c.check['omp'] = New-OmpResult $true $false $true 'bridge not listening (OMP_BRIDGE_DOWN)'
    $c.setup['omp'] = New-OmpResult $true $false $true 'bridge not listening (OMP_BRIDGE_DOWN)'
} {
    param($r)
    Assert-Exit $r 4
    Assert-Says $r 'RESULT: installed-omp-incomplete'
    Assert-Says $r 'bridge=NOT connected'
    Assert-Says $r 'OMP_BRIDGE_DOWN'
    Assert-Says $r 'not fully connected'
    Assert-NotSays $r 'RESULT: ready'
}

Invoke-Case 'omp partial: all three true but error present is still incomplete' {
    param($c)
    $c.args = @{ WithOmp = $true }
    $c.check['omp'] = New-OmpResult $true $true $true 'observer conflict (OBSERVER_CONFLICT)'
} {
    param($r)
    Assert-Exit $r 4
    Assert-Says $r 'OBSERVER_CONFLICT'
}

Invoke-Case 'omp connect error is not hidden by a healthy status check' {
    param($c)
    $c.args = @{ WithOmp = $true }
    $c.check['omp'] = New-OmpResult $true $true $true $null
    $c.setup['omp'] = New-OmpResult $true $true $true 'connect failed (OBSERVER_CONFLICT)'
} {
    param($r)
    Assert-Exit $r 4
    Assert-Says $r 'OBSERVER_CONFLICT'
    Assert-NotSays $r 'RESULT: ready'
}

Invoke-Case 'omp partial: observer missing' {
    param($c)
    $c.args = @{ WithOmp = $true }
    $c.check['omp'] = New-OmpResult $true $true $false $null
} {
    param($r)
    Assert-Exit $r 4
    Assert-Says $r 'observer=NOT connected'
}

Invoke-Case 'omp requested but absent from the result is incomplete, not ready' {
    param($c)
    $c.args = @{ WithOmp = $true }
} {
    param($r)
    Assert-Exit $r 4
    Assert-Says $r 'no omp result'
}

Invoke-Case 'omp detected but not requested is left alone' {
    param($c)
    $c.check['omp'] = New-OmpResult $false $false $false $null
} {
    param($r)
    Assert-Exit $r 0
    Assert-Says $r 'detected but not requested'
    $calls = @(Read-Log $r 'aam-calls.log') | ForEach-Object { $_.Split('|')[1] }
    Assert-That (-not ($calls -contains 'setup --with-omp')) 'must not connect omp unless asked'
}

Invoke-Case 'omp not requested and not installed' { param($c) } {
    param($r)
    Assert-Exit $r 0
    Assert-Says $r 'omp: not requested'
}

Invoke-Case 'setup problem plus omp: base failure wins (exit 3)' {
    param($c)
    $c.args = @{ WithOmp = $true }
    $c.aam = @{ version = '0.5.8'; setup_exit = '1'; check_exit = '0' }
    $c.setupOut = ''
    $c.setupErr = 'aam: failed (SETUP_FAILED)'
    $c.check['omp'] = New-OmpResult $true $true $true $null
} {
    param($r)
    Assert-Exit $r 3
    Assert-Says $r 'SETUP_FAILED'
}

# ------------------------------------------------------------ report

try { Remove-Item -LiteralPath $root -Recurse -Force -ErrorAction Stop } catch { Write-Host ('note: could not remove ' + $root) }

Write-Host ''
Write-Host ('assertions passed: ' + $script:Passed)
if ($script:Failures.Count -gt 0) {
    Write-Host ('FAILED: ' + $script:Failures.Count)
    foreach ($f in $script:Failures) { Write-Host ('  - ' + $f) }
    exit 1
}
Write-Host 'ALL PASSED'
exit 0
