# Run over elevated SSH after preserving any wanted logs/dumps.
$ErrorActionPreference = 'Stop'
$root = 'C:\Users\Daniel\dev\latch-rfi2'
$task = Get-ScheduledTask -TaskName 'LatchTestRfiMF' -ErrorAction SilentlyContinue
if ($task) {
    Stop-ScheduledTask -InputObject $task
    Unregister-ScheduledTask -InputObject $task -Confirm:$false
}
# Stop only binaries in this workstream's directory and rustc invocations
# explicitly targeting it; never match another checkout or installed host.
Get-CimInstance Win32_Process | Where-Object {
    ($_.ExecutablePath -and $_.ExecutablePath.StartsWith("$root\", [StringComparison]::OrdinalIgnoreCase)) -or
    ($_.Name -eq 'rustc.exe' -and $_.CommandLine -like "*$root*")
} | ForEach-Object { Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }
# Application-path matching also removes any automatically generated Block rules.
Get-NetFirewallApplicationFilter | Where-Object {
    $_.Program -and $_.Program.StartsWith("$root\", [StringComparison]::OrdinalIgnoreCase)
} | Get-NetFirewallRule | Remove-NetFirewallRule
Remove-NetFirewallRule -DisplayName 'LatchTest rfi-vt-mf*' -ErrorAction SilentlyContinue
if (Test-Path $root) { Remove-Item -LiteralPath $root -Recurse -Force }
if (Test-Path $root) { throw 'build directory remains' }
'RFI workstream cleanup complete'
