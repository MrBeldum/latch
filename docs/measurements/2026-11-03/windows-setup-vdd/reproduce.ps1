$ErrorActionPreference='Stop'
$ProgressPreference='SilentlyContinue'
$root='C:\Users\Daniel\dev\latch-fix-vdd'
$reg='HKLM:\SOFTWARE\MikeTheTech\VirtualDisplayDriver'
$file='C:\VirtualDisplayDriver\vdd_settings.xml'
$bytes=[IO.File]::ReadAllBytes($file)
[IO.File]::WriteAllBytes("$root\original.xml",$bytes)
$old=(Get-ItemProperty $reg).VDDPATH
$log=(Get-ItemProperty $reg).LoggingEnabled
$debug=(Get-ItemProperty $reg).DebugLoggingEnabled
function Monitor-State($label) {
  $m=@(Get-PnpDevice -Class Monitor | Where-Object InstanceId -like 'DISPLAY\MTT1337\*')
  "$label elapsed=$([math]::Round($clock.Elapsed.TotalSeconds,1)) monitors=$($m.Status -join ',')"
}
function Save-Xml($xml,$path) {
  $s=New-Object Xml.XmlWriterSettings
  $s.Indent=$true
  $s.Encoding=New-Object Text.UTF8Encoding($false)
  $w=[Xml.XmlWriter]::Create($path,$s)
  try {$xml.Save($w)} finally {$w.Close()}
}
function Restart-Vdd {
 pnputil /restart-device 'ROOT\DISPLAY\0000'
}
try {
 New-ItemProperty $reg LoggingEnabled -Value 1 -PropertyType DWord -Force | Out-Null
 New-ItemProperty $reg DebugLoggingEnabled -Value 1 -PropertyType DWord -Force | Out-Null
 foreach($count in @(102,98)) {
  [xml]$xml=[Text.Encoding]::UTF8.GetString($bytes)
  foreach($r in $xml.vdd_settings.resolutions.resolution) {
   if("$($r.width)x$($r.height)" -in @('1920x1080','2560x1440')) {
    foreach($hz in @(90,144,165,240,360)) {$e=$xml.CreateElement('refresh_rate');$e.InnerText="$hz";[void]$r.AppendChild($e)}
   }
  }
  # The original bug appended both large sizes twice.
  $copies=if($count -eq 102){2}else{1}
  foreach($copy in 1..$copies) {foreach($h in @(4320,2160)) {
   $r=$xml.CreateElement('resolution');$r.InnerXml="<width>7680</width><height>$h</height><refresh_rate>60</refresh_rate><refresh_rate>120</refresh_rate>";[void]$xml.vdd_settings.resolutions.AppendChild($r)
  }}
  "TEST $count computed=$(@($xml.SelectNodes('//refresh_rate')).Count)"
  # First test the same Program Files relocation as setup.
  $dest='C:\Program Files\Latch\Display'
  New-Item -ItemType Directory -Force $dest | Out-Null
  Save-Xml $xml "$dest\vdd_settings.xml"
  Save-Xml $xml "$root\candidate-$count.xml"
  New-ItemProperty $reg VDDPATH -Value $dest -PropertyType String -Force | Out-Null
  $clock=[Diagnostics.Stopwatch]::StartNew()
  Restart-Vdd
  foreach($i in 1..12) {Start-Sleep -Seconds 2;Monitor-State "relocated $count"}
  Get-ChildItem "$dest\Logs" -ErrorAction SilentlyContinue | ForEach-Object {Copy-Item $_.FullName "$root\relocated-$count-driver.log" -Force}
  # Reset before next trial, including an explicit restart.
  if($old){Set-ItemProperty $reg VDDPATH $old}else{Remove-ItemProperty $reg VDDPATH -ErrorAction SilentlyContinue}
  Restart-Vdd
  Start-Sleep -Seconds 3
  Monitor-State 'restored between tests'
 }
} finally {
 [IO.File]::WriteAllBytes($file,$bytes)
 foreach($p in @(@('VDDPATH',$old),@('LoggingEnabled',$log),@('DebugLoggingEnabled',$debug))) {
  if($null -ne $p[1]){Set-ItemProperty $reg $p[0] $p[1]}else{Remove-ItemProperty $reg $p[0] -ErrorAction SilentlyContinue}
 }
 Restart-Vdd
 Start-Sleep -Seconds 4
 Monitor-State 'FINAL restored'
 Get-PnpDevice -Class Display,Monitor | Format-Table Status,Class,FriendlyName,InstanceId -AutoSize
}
