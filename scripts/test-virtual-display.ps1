# Run elevated with the script dumped by LATCH_DUMP_VDD_SCRIPT. The fixture
# shadows device/registry discovery; no real display or registry is changed.
param([Parameter(Mandatory=$true)][string]$SetupScript)
$ErrorActionPreference = 'Stop'
$scriptFile = (Resolve-Path $SetupScript).Path
$fixture = Join-Path $env:ProgramFiles ('LatchVddSelftest-' + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $fixture | Out-Null
$file = Join-Path $fixture 'vdd_settings.xml'
$global:vddTestAlive = $true
$global:vddTestReject = $false
function Get-ItemProperty { param($Path) [pscustomobject]@{ VDDPATH = $fixture } }
function Get-PnpDevice {
    [CmdletBinding()]param([string]$Class)
    if ($Class -eq 'Display') { [pscustomobject]@{ FriendlyName = 'Virtual Display Driver'; Status = 'OK'; InstanceId = 'fixture' } }
    if ($Class -eq 'Monitor' -and $global:vddTestAlive) { [pscustomobject]@{ Status = 'OK'; InstanceId = 'DISPLAY\MTT1337\fixture' } }
}
function pnputil { if ($global:vddTestReject) { $global:vddTestAlive = $false } }
try {
    [IO.File]::WriteAllText($file, '<vdd_settings><resolutions><resolution><width>1920</width><height>1080</height><refresh_rate>59.95</refresh_rate></resolution></resolutions><colour><HDRPlus>false</HDRPlus></colour></vdd_settings>')
    & $scriptFile
    [xml]$xml = Get-Content -LiteralPath $file -Raw
    $mode = $xml.vdd_settings.resolutions.resolution | Where-Object { $_.width -eq '1920' -and $_.height -eq '1080' }
    foreach ($hz in @('59.95', '60', '120', '240', '360')) { if ($hz -notin @($mode.refresh_rate)) { throw "missing $hz Hz" } }
    if ($xml.vdd_settings.colour.HDRPlus -ne 'false') { throw 'existing settings lost' }
    $hash = (Get-FileHash $file).Hash
    & $scriptFile
    if ($hash -ne (Get-FileHash $file).Hash) { throw 'second setup changed the file' }
    # Remove a required timing, then simulate monitor-arrival failure.
    $mode.SelectSingleNode("refresh_rate[text()='240']") | ForEach-Object { [void]$mode.RemoveChild($_) }
    $xml.Save($file)
    $hash = (Get-FileHash $file).Hash
    $global:vddTestReject = $true
    $rejected = $false
    try { & $scriptFile } catch { $rejected = $_ -like '*previous settings restored*' }
    if (-not $rejected -or $hash -ne (Get-FileHash $file).Hash) { throw 'rejected configuration was not rolled back' }
    Write-Output 'PASS: refresh upgrade, custom timing preservation, idempotence, rollback'
} finally {
    Remove-Item -LiteralPath $fixture -Recurse -Force
    Remove-Variable vddTestAlive,vddTestReject -Scope Global
}
