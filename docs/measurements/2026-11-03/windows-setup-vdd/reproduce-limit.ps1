$ErrorActionPreference='Stop'
$ProgressPreference='SilentlyContinue'
$root='C:\Users\Daniel\dev\latch-fix-vdd'
$reg='HKLM:\SOFTWARE\MikeTheTech\VirtualDisplayDriver'
$file='C:\VirtualDisplayDriver\vdd_settings.xml'
$bytes=[IO.File]::ReadAllBytes($file)
[IO.File]::WriteAllBytes("$root\original.xml",$bytes)
$old=(Get-ItemProperty $reg).VDDPATH
$log=(Get-ItemProperty $reg).LOGS
$debug=(Get-ItemProperty $reg).DEBUGLOGS
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
 New-ItemProperty $reg LOGS -Value 1 -PropertyType DWord -Force | Out-Null
 New-ItemProperty $reg DEBUGLOGS -Value 1 -PropertyType DWord -Force | Out-Null
 foreach($count in @(100,101)) {
  [xml]$xml=[Text.Encoding]::UTF8.GetString($bytes)
  foreach($r in $xml.vdd_settings.resolutions.resolution) {
   if("$($r.width)x$($r.height)" -in @('1920x1080','2560x1440')) {
    foreach($hz in @(90,144,165,240,360)) {$e=$xml.CreateElement('refresh_rate');$e.InnerText="$hz";[void]$r.AppendChild($e)}
   }
  }
  $copies=1
  foreach($copy in 1..$copies) {foreach($h in @(4320,2160)) {
   $r=$xml.CreateElement('resolution');$r.InnerXml="<width>7680</width><height>$h</height><refresh_rate>60</refresh_rate><refresh_rate>120</refresh_rate>";[void]$xml.vdd_settings.resolutions.AppendChild($r)
  }}
  if($count -eq 100) {
   $r=$xml.CreateElement('resolution');$r.InnerXml='<width>1024</width><height>768</height><refresh_rate>60</refresh_rate><refresh_rate>120</refresh_rate>';[void]$xml.vdd_settings.resolutions.AppendChild($r)
  } else {
   $r=$xml.SelectSingleNode('/vdd_settings/resolutions/resolution[width="1920" and height="1080"]')
   foreach($hz in @(75,100,200)) {$e=$xml.CreateElement('refresh_rate');$e.InnerText="$hz";[void]$r.AppendChild($e)}
  }
  $e=$xml.CreateElement('logging');$e.InnerText='true';[void]$xml.DocumentElement.AppendChild($e)
  $e=$xml.CreateElement('debuglogging');$e.InnerText='true';[void]$xml.DocumentElement.AppendChild($e)
  "TEST $count computed=$(@($xml.SelectNodes('//refresh_rate')).Count)"
  # First test the same Program Files relocation as setup.
  $dest='C:\Program Files\Latch\Display'
  New-Item -ItemType Directory -Force $dest | Out-Null
  New-Item -ItemType Directory -Force "$dest\Logs" | Out-Null
  icacls "$dest\Logs" /grant '*S-1-5-19:(OI)(CI)M' | Out-Null
  Save-Xml $xml "$dest\vdd_settings.xml"
  Save-Xml $xml "$root\candidate-$count.xml"
  New-ItemProperty $reg VDDPATH -Value $dest -PropertyType String -Force | Out-Null
  $clock=[Diagnostics.Stopwatch]::StartNew()
  Restart-Vdd
  foreach($i in 1..4) {Start-Sleep -Seconds 2;Monitor-State "relocated $count"}
  Get-ChildItem "$dest\Logs" -ErrorAction SilentlyContinue | ForEach-Object {Copy-Item $_.FullName "$root\relocated-$count-driver.log" -Force}
  # Reset before next trial, including an explicit restart.
  if($old){Set-ItemProperty $reg VDDPATH $old}else{Remove-ItemProperty $reg VDDPATH -ErrorAction SilentlyContinue}
  Restart-Vdd
  Start-Sleep -Seconds 3
  Monitor-State 'restored between tests'
 }
} finally {
 [IO.File]::WriteAllBytes($file,$bytes)
 foreach($p in @(@('VDDPATH',$old),@('LOGS',$log),@('DEBUGLOGS',$debug))) {
  if($null -ne $p[1]){Set-ItemProperty $reg $p[0] $p[1]}else{Remove-ItemProperty $reg $p[0] -ErrorAction SilentlyContinue}
 }
 Restart-Vdd
 Start-Sleep -Seconds 4
 Monitor-State 'FINAL restored'
 Get-PnpDevice -Class Display,Monitor | Format-Table Status,Class,FriendlyName,InstanceId -AutoSize
}
