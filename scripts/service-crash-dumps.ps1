#Requires -Version 5.1
[CmdletBinding()]
param(
    [ValidateSet('Status', 'Enable', 'Disable')]
    [string]$Mode = 'Status',
    [string]$DumpDirectory = (Join-Path $env:APPDATA 'AI Account Manager\crash-dumps')
)

$ErrorActionPreference = 'Stop'
$Key = 'HKLM:\SOFTWARE\Microsoft\Windows\Windows Error Reporting\LocalDumps\aam-service.exe'
$Owner = 'Ojak.LocalDumps.v1'
$DumpDirectory = [IO.Path]::GetFullPath($DumpDirectory)

function Assert-RealPath([string]$Path) {
    $cursor = $Path
    while ($cursor) {
        if (Test-Path -LiteralPath $cursor) {
            $item = Get-Item -LiteralPath $cursor -Force
            if (($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0 -or -not $item.PSIsContainer) {
                throw 'Dump directory must contain only real directories, not links or junctions.'
            }
        }
        $parent = [IO.Path]::GetDirectoryName($cursor.TrimEnd('\'))
        if ($parent -eq $cursor) { break }
        $cursor = $parent
    }
}

function Read-Configuration {
    if (-not (Test-Path -LiteralPath $Key)) { return $null }
    Get-ItemProperty -LiteralPath $Key
}

function Assert-OwnedConfiguration($Configuration) {
    if ($Configuration.OjakOwner -ne $Owner -or $Configuration.DumpFolder -ne $DumpDirectory -or
        $Configuration.DumpType -ne 1 -or $Configuration.DumpCount -ne 3) {
        throw 'Existing LocalDumps configuration is not the unchanged Ojak configuration. Nothing was overwritten.'
    }
    $registry = Get-Item -LiteralPath $Key
    $unknown = @($registry.GetValueNames() | Where-Object { $_ -notin @('OjakOwner', 'DumpFolder', 'DumpType', 'DumpCount') })
    if ($unknown.Count -gt 0 -or $registry.SubKeyCount -gt 0) {
        throw 'LocalDumps configuration has additional settings. Nothing was removed.'
    }
}

$existing = Read-Configuration
if ($Mode -eq 'Status') {
    if ($null -eq $existing) {
        [pscustomobject]@{ Enabled = $false; Application = 'aam-service.exe' }
    } else {
        [pscustomobject]@{
            Enabled = $true
            Application = 'aam-service.exe'
            DumpFolder = $existing.DumpFolder
            DumpType = $existing.DumpType
            DumpCount = $existing.DumpCount
            ManagedByOjak = ($existing.OjakOwner -eq $Owner)
        }
    }
    return
}

$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
$principal = [Security.Principal.WindowsPrincipal]::new($identity)
if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw 'Administrator approval is required for per-application WER settings. Run this script from an elevated PowerShell window.'
}
if ($null -ne $existing) { Assert-OwnedConfiguration $existing }

if ($Mode -eq 'Disable') {
    if ($null -ne $existing) { Remove-Item -LiteralPath $Key }
    Write-Output 'Ojak local dump collection disabled. Existing dump files were preserved.'
    return
}

Assert-RealPath $DumpDirectory
if (-not (Test-Path -LiteralPath $DumpDirectory)) {
    $initialAcl = [Security.AccessControl.DirectorySecurity]::new()
    $initialAcl.SetOwner($identity.User)
    $initialAcl.SetAccessRuleProtection($true, $false)
    foreach ($sid in @($identity.User, [Security.Principal.SecurityIdentifier]::new('S-1-5-18'))) {
        $initialAcl.AddAccessRule([Security.AccessControl.FileSystemAccessRule]::new(
            $sid, 'FullControl', 'ContainerInherit, ObjectInherit', 'None', 'Allow'))
    }
    [IO.Directory]::CreateDirectory($DumpDirectory, $initialAcl) | Out-Null
}
Assert-RealPath $DumpDirectory
$acl = Get-Acl -LiteralPath $DumpDirectory
if ($acl.GetOwner([Security.Principal.SecurityIdentifier]).Value -ne $identity.User.Value) {
    throw 'Dump directory is not owned by the current user. Its permissions were not changed.'
}
$acl.SetAccessRuleProtection($true, $false)
foreach ($rule in @($acl.Access)) { [void]$acl.RemoveAccessRuleSpecific($rule) }
$inherit = [Security.AccessControl.InheritanceFlags]'ContainerInherit, ObjectInherit'
$propagation = [Security.AccessControl.PropagationFlags]::None
$allow = [Security.AccessControl.AccessControlType]::Allow
foreach ($sid in @($identity.User, [Security.Principal.SecurityIdentifier]::new('S-1-5-18'))) {
    $acl.AddAccessRule([Security.AccessControl.FileSystemAccessRule]::new($sid, 'FullControl', $inherit, $propagation, $allow))
}
Set-Acl -LiteralPath $DumpDirectory -AclObject $acl

if ($null -eq $existing) { New-Item -Path $Key -Force | Out-Null }
New-ItemProperty -LiteralPath $Key -Name OjakOwner -Value $Owner -PropertyType String -Force | Out-Null
New-ItemProperty -LiteralPath $Key -Name DumpFolder -Value $DumpDirectory -PropertyType ExpandString -Force | Out-Null
New-ItemProperty -LiteralPath $Key -Name DumpType -Value 1 -PropertyType DWord -Force | Out-Null
New-ItemProperty -LiteralPath $Key -Name DumpCount -Value 3 -PropertyType DWord -Force | Out-Null
Assert-OwnedConfiguration (Read-Configuration)
Write-Output 'Enabled local minidumps for aam-service.exe only (maximum 3). Dumps may contain credentials or prompts: keep them private; never upload automatically.'
