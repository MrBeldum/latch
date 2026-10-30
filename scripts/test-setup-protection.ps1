# Isolated Windows regression: reject junctions, user-writable folders and
# log symlinks before any elevated file write. No real install is touched.
$ErrorActionPreference = 'Stop'
. "$PSScriptRoot\setup-protection.ps1"
$root = Join-Path $env:RUNNER_TEMP ([guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $root | Out-Null
try {
    $acl = New-Object Security.AccessControl.DirectorySecurity
    $acl.SetSecurityDescriptorSddlForm('O:BAG:BAD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;FRFX;;;BU)')
    Set-Acl -LiteralPath $root -AclObject $acl
    Assert-Protected $root
    $unsafe = Join-Path $root 'writable'
    New-Item -ItemType Directory -Path $unsafe | Out-Null
    $acl = Get-Acl -LiteralPath $unsafe
    $users = [Security.Principal.SecurityIdentifier]'S-1-5-32-545'
    $acl.AddAccessRule((New-Object Security.AccessControl.FileSystemAccessRule($users, 'Modify', 'Allow')))
    Set-Acl -LiteralPath $unsafe -AclObject $acl
    function Must-Reject($path) {
        $rejected = $false
        try { Assert-Protected $path } catch { $rejected = $true }
        if (-not $rejected) { throw "Accepted unsafe path: $path" }
    }
    Must-Reject $unsafe
    $victim = Join-Path $root 'victim.txt'
    Set-Content -LiteralPath $victim -Value 'unchanged'
    $link = Join-Path $root 'setup.log'
    New-Item -ItemType SymbolicLink -Path $link -Target $victim | Out-Null
    Must-Reject $link
    if ((Get-Content -LiteralPath $victim) -ne 'unchanged') { throw 'Victim was changed' }
    Remove-Item -LiteralPath $link -Force
    $junction = Join-Path $root 'junction'
    New-Item -ItemType Junction -Path $junction -Target $unsafe | Out-Null
    Must-Reject $junction
    [IO.Directory]::Delete($junction)
} finally {
    Remove-Item -LiteralPath $root -Recurse -Force
}
