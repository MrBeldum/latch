$ErrorActionPreference='Stop'
$ProgressPreference='SilentlyContinue'
$root='C:\Users\Daniel\dev\latch-fix-vdd'
$setup='C:\Program Files\Latch\Setup\latch-host.exe'
$installed='C:\Users\Daniel\AppData\Local\Latch\latch-host.exe'
$reg='HKLM:\SOFTWARE\MikeTheTech\VirtualDisplayDriver'
# Save the helper as setup refreshes it; the deployed host itself stays running.
Copy-Item 'C:\Program Files\Latch\Input\latch-host.exe' "$root\input-main-backup.exe" -Force
$hostBefore=Get-Process latch-host | Where-Object Path -eq $installed
$hostBefore.Id | Set-Content "$root\installed-pid-before.txt"
Copy-Item "$root\target\release\latch-host.exe" $setup -Force
if((Get-FileHash "$root\target\release\latch-host.exe").Hash -ne (Get-FileHash $setup).Hash) {throw 'staging hash mismatch'}
$bytes=[IO.File]::ReadAllBytes('C:\VirtualDisplayDriver\vdd_settings.xml')
function Run-Setup($label) {
 $command='"' + $setup + '" --setup-elevated --program "' + $installed + '" > "' + "$root\$label-console.log" + '" 2>&1'
 [IO.File]::WriteAllText("$root\setup-command.cmd", "@echo off`r`n$command`r`nexit /b %errorlevel%`r`n")
 & cmd.exe /d /c "$root\setup-command.cmd"
 $code=$LASTEXITCODE
 Copy-Item 'C:\Program Files\Latch\Setup\setup.log' "$root\$label-setup.log" -Force
 "EXIT $label $code"
 Get-Content "$root\$label-setup.log"
 if($code -ne 0){throw "setup exited $code"}
}
try {
 # 84 existing + four extra custom modes + fourteen from setup = 102.
 # Rejection must restore this valid 88-mode input and complete setup.
 [xml]$xml=[Text.Encoding]::UTF8.GetString($bytes)
 foreach($pair in @(@(1024,768),@(854,480))) {
  $r=$xml.CreateElement('resolution');$r.InnerXml="<width>$($pair[0])</width><height>$($pair[1])</height><refresh_rate>60</refresh_rate><refresh_rate>120</refresh_rate>";[void]$xml.vdd_settings.resolutions.AppendChild($r)
 }
 $xml.Save('C:\VirtualDisplayDriver\vdd_settings.xml')
 $input=[IO.File]::ReadAllBytes('C:\VirtualDisplayDriver\vdd_settings.xml')
 # Remove research-only diagnostic directory; setup must see the original
 # stale empty Display directory left by the failed deployed setup.
 Remove-Item 'C:\Program Files\Latch\Display' -Recurse -Force -ErrorAction SilentlyContinue
 New-Item -ItemType Directory 'C:\Program Files\Latch\Display' | Out-Null
 Run-Setup 'rejected'
 if((Get-ItemProperty $reg).VDDPATH){throw 'rejected relocation remains'}
 if(Test-Path 'C:\Program Files\Latch\Display'){throw 'stale Display folder remains'}
 if([Convert]::ToBase64String([IO.File]::ReadAllBytes('C:\VirtualDisplayDriver\vdd_settings.xml')) -ne [Convert]::ToBase64String($input)){throw 'input XML changed'}
} finally {
 [IO.File]::WriteAllBytes('C:\VirtualDisplayDriver\vdd_settings.xml',$bytes)
 Remove-ItemProperty $reg VDDPATH -ErrorAction SilentlyContinue
 pnputil /restart-device 'ROOT\DISPLAY\0000' | Out-Host
}
Run-Setup 'accepted'
Run-Setup 'idempotent'
$hostAfter=Get-Process -Id $hostBefore.Id
"Installed host still running: $($hostAfter.Id) $($hostAfter.Path)"
