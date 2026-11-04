$ErrorActionPreference='Stop'
$ProgressPreference='SilentlyContinue'
$root='C:\Users\Daniel\dev\latch-fix-vdd'
foreach($task in @('LatchVddProbe','LatchVddMotion','LatchVddServe')) {
 Stop-ScheduledTask $task -ErrorAction SilentlyContinue
 Unregister-ScheduledTask $task -Confirm:$false -ErrorAction SilentlyContinue
}
Get-CimInstance Win32_Process | Where-Object {$_.Name -eq 'powershell.exe' -and ($_.CommandLine.EndsWith('-File C:\Users\Daniel\dev\latch-fix-vdd\motion.ps1') -or $_.CommandLine.EndsWith('-File C:\Users\Daniel\dev\latch-fix-vdd\live-stream.ps1'))} | ForEach-Object {Stop-Process -Id $_.ProcessId -ErrorAction SilentlyContinue}
$server=Get-Process -Id ([int](Get-Content "$root\serve.pid")) -ErrorAction SilentlyContinue
if($server -and $server.Path -eq "$root\target\release\examples\serve.exe") {Stop-Process -Id $server.Id}
Get-NetFirewallRule -DisplayName LatchVddTest -ErrorAction SilentlyContinue | Remove-NetFirewallRule
[IO.File]::WriteAllBytes('C:\VirtualDisplayDriver\vdd_settings.xml',[IO.File]::ReadAllBytes("$root\original.xml"))
Remove-ItemProperty 'HKLM:\SOFTWARE\MikeTheTech\VirtualDisplayDriver' VDDPATH -ErrorAction SilentlyContinue
Remove-Item 'HKLM:\SOFTWARE\Latch\Display' -Force -ErrorAction SilentlyContinue
pnputil /restart-device 'ROOT\DISPLAY\0000'
foreach($i in 1..60) {
 Start-Sleep -Milliseconds 250
 if(@(Get-PnpDevice -Class Monitor | Where-Object {$_.InstanceId -like 'DISPLAY\MTT1337\*' -and $_.Status -eq 'OK'}).Count -gt 0){break}
}
Remove-Item 'C:\Program Files\Latch\Display' -Recurse -Force -ErrorAction SilentlyContinue
Stop-ScheduledTask LatchInput -ErrorAction SilentlyContinue
Get-Process latch-host | Where-Object Path -eq 'C:\Program Files\Latch\Input\latch-host.exe' | Stop-Process -Force
Start-Sleep -Milliseconds 500
Copy-Item "$root\input-main-backup.exe" 'C:\Program Files\Latch\Input\latch-host.exe' -Force
Start-ScheduledTask LatchInput
Remove-Item 'C:\Program Files\Latch\Setup\latch-host.exe' -Force
Get-PnpDevice -Class Display,Monitor | Format-Table Status,Class,FriendlyName,InstanceId -AutoSize
"original XML preserved: $((Get-FileHash C:\VirtualDisplayDriver\vdd_settings.xml).Hash -eq (Get-FileHash "$root\original.xml").Hash)"
"VDDPATH: $((Get-ItemProperty 'HKLM:\SOFTWARE\MikeTheTech\VirtualDisplayDriver').VDDPATH)"
"Display folder remains: $(Test-Path 'C:\Program Files\Latch\Display')"
"Input helper restored: $((Get-FileHash 'C:\Program Files\Latch\Input\latch-host.exe').Hash -eq (Get-FileHash "$root\input-main-backup.exe").Hash)"
$installed=Get-Process -Id ([int](Get-Content "$root\installed-pid-before.txt"))
"Installed process: $($installed.Id) $($installed.Path) session=$($installed.SessionId)"
Get-FileHash 'C:\Users\Daniel\AppData\Local\Latch\latch-host.exe','C:\Program Files\Latch\Input\latch-host.exe'
