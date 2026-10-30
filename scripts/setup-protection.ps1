# Run only elevated. Refuse pre-existing paths a non-administrator can
# redirect or change; changing their ACL would leave existing handles open.
$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
if (-not ([Security.Principal.WindowsPrincipal]$identity).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw 'Setup requires an administrator'
}
$trusted = @('S-1-5-18', 'S-1-5-32-544', $identity.User.Value,
    'S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464') # TrustedInstaller
function Assert-Protected($path) {
    $item = Get-Item -LiteralPath $path -Force -ErrorAction Stop
    if ($item.Attributes -band [IO.FileAttributes]::ReparsePoint) { throw "Reparse point: $path" }
    $acl = Get-Acl -LiteralPath $path -ErrorAction Stop
    if ($acl.GetOwner([Security.Principal.SecurityIdentifier]).Value -notin $trusted) { throw "Untrusted owner: $path" }
    $writes = [Security.AccessControl.FileSystemRights]'Write, Delete, DeleteSubdirectoriesAndFiles, ChangePermissions, TakeOwnership'
    foreach ($rule in $acl.GetAccessRules($true, $true, [Security.Principal.SecurityIdentifier])) {
        if ($rule.PropagationFlags -band [Security.AccessControl.PropagationFlags]::InheritOnly) { continue }
        if ($rule.AccessControlType -eq 'Allow' -and ($rule.FileSystemRights -band $writes) -and $rule.IdentityReference.Value -notin $trusted) {
            throw "Untrusted write access: $path"
        }
    }
}
