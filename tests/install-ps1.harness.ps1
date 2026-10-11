<#
Child-session harness for tests/install-ps1.tests.ps1. Not a test by itself.

It runs the REAL install.ps1 (param binding, main flow, exit code) after putting a
few fixture seams in front of the commands that would touch the network, the
registry, and the process table. Every function here shadows a cmdlet by name
for this session only; install.ps1 is not modified or sourced piecemeal.
#>
param([Parameter(Mandatory = $true)][string]$ScenarioPath)

$ErrorActionPreference = 'Stop'
$global:Sc = Get-Content -LiteralPath $ScenarioPath -Raw -Encoding UTF8 | ConvertFrom-Json
$global:Closed = $false
$global:SleepMs = 0

# Isolation: this child session owns its environment. Nothing leaks to the caller.
$env:OJAK_FX_DIR = $global:Sc.fxDir
$env:LOCALAPPDATA = $global:Sc.localAppData
$env:TEMP = (Join-Path $global:Sc.fxDir 'tmp')
$env:TMP = $env:TEMP
[void][IO.Directory]::CreateDirectory($env:TEMP)
foreach ($property in $global:Sc.env.PSObject.Properties) {
    if ($null -eq $property.Value) { Remove-Item -LiteralPath ('Env:' + $property.Name) -ErrorAction SilentlyContinue }
    else { Set-Item -LiteralPath ('Env:' + $property.Name) -Value ([string]$property.Value) }
}

function global:Add-FxLog {
    param([string]$File, [string]$Line)
    [IO.File]::AppendAllText((Join-Path $global:Sc.fxDir $File), $Line + "`r`n")
}

function global:Get-OjakTestElevation { return [bool]$global:Sc.elevated }

# Network: only URLs listed in the scenario resolve. Everything else is a 404.
function global:Invoke-WebRequest {
    [CmdletBinding()]
    param([switch]$UseBasicParsing, [string]$Uri, [string]$UserAgent, [int]$TimeoutSec, [int]$MaximumRedirection, $Headers, [string]$OutFile)
    Add-FxLog 'web.log' $Uri
    $route = $global:Sc.routes.PSObject.Properties[$Uri]
    if ($null -eq $route) { throw ('404 not found: ' + $Uri) }
    if ($OutFile) {
        Copy-Item -LiteralPath $route.Value -Destination $OutFile -Force
        return
    }
    return [pscustomobject]@{ StatusCode = 200; Content = [IO.File]::ReadAllText($route.Value, [Text.Encoding]::UTF8) }
}

# Registry: the fake installer writes registry.txt (line 1 version, line 2 folder).
function global:Get-ItemProperty {
    [CmdletBinding()]
    param([string]$LiteralPath)
    $reg = Join-Path $global:Sc.fxDir 'registry.txt'
    if (-not (Test-Path -LiteralPath $reg)) { return $null }
    $lines = @(Get-Content -LiteralPath $reg -Encoding UTF8)
    if ($LiteralPath -like '*lapal\Ojak') { return [pscustomobject]@{ '(default)' = $lines[1] } }
    if ($LiteralPath -like '*Uninstall\Ojak') { return [pscustomobject]@{ DisplayVersion = $lines[0]; InstallLocation = ('"' + $lines[1] + '"') } }
    return $null
}

# Process table: this session, its parent, plus the scenario's rows.
function global:Get-CimInstance {
    [CmdletBinding()]
    param([string]$ClassName, [string[]]$Property)
    $install = Join-Path $global:Sc.localAppData 'Ojak'
    $rows = @()
    $rows += [pscustomobject]@{ ProcessId = [uint32]$PID; ParentProcessId = [uint32]5000; Name = 'powershell.exe'; ExecutablePath = 'C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe' }
    $rows += [pscustomobject]@{ ProcessId = [uint32]5000; ParentProcessId = [uint32]1; Name = 'terminal.exe'; ExecutablePath = 'C:\Tools\terminal.exe' }
    foreach ($p in @($global:Sc.processes)) {
        if ($null -eq $p) { continue }
        if ($global:Closed -and $p.PSObject.Properties['goneAfterClose'] -and $p.goneAfterClose) { continue }
        if ($p.PSObject.Properties['asParent'] -and $p.asParent) {
            # This script's own parent (the terminal that launched it) is the Ojak program.
            $rows[1].Name = [string]$p.name
            $rows[1].ExecutablePath = ([string]$p.path).Replace('{INSTALL}', $install)
            continue
        }
        $rows += [pscustomobject]@{
            ProcessId = [uint32]$p.pid
            ParentProcessId = [uint32]$p.parent
            Name = [string]$p.name
            ExecutablePath = ([string]$p.path).Replace('{INSTALL}', $install)
        }
    }
    return $rows
}

function global:Get-Process {
    [CmdletBinding()]
    param([int]$Id)
    $o = [pscustomobject]@{ Id = $Id; MainWindowHandle = [IntPtr]1 }
    Add-Member -InputObject $o -MemberType ScriptMethod -Name CloseMainWindow -Value {
        Add-FxLog 'calls.log' ('CloseMainWindow pid=' + $this.Id)
        if ($global:Sc.closeRemoves) { $global:Closed = $true }
        return $true
    }
    return $o
}

# The installer must never force-stop anything. Any call is recorded and fails the run.
function global:Stop-Process {
    [CmdletBinding()]
    param([Parameter(ValueFromRemainingArguments = $true)]$Rest)
    Add-FxLog 'calls.log' 'FORCE-KILL Stop-Process'
    throw 'Stop-Process must not be called'
}

# Fake clock so the 30 s "ask Ojak to close" wait does not take 30 real seconds.
function global:Start-Sleep {
    [CmdletBinding()]
    param([int]$Milliseconds, [int]$Seconds)
    $global:SleepMs += $Milliseconds + 1000 * $Seconds
}
function global:Get-Date {
    return [DateTime]::Now.AddMilliseconds($global:SleepMs)
}

$splat = @{}
foreach ($property in $global:Sc.args.PSObject.Properties) { $splat[$property.Name] = $property.Value }
& $global:Sc.scriptPath @splat
$code = $LASTEXITCODE
Write-Host ('HARNESS_EXIT=' + $code)
exit $code
