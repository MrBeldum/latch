# Exercise the generated step against isolated files and registry keys;
# mock PnP only, so no display or installed driver is touched.
param([Parameter(Mandatory)][string]$Script)
$ErrorActionPreference = 'Stop'
$source = [IO.File]::ReadAllText((Resolve-Path $Script))
$source = $source.Substring($source.IndexOf('Step "Listing virtual display'))
$fixture = Join-Path ([IO.Path]::GetTempPath()) ('LatchVdd-' + [guid]::NewGuid().ToString('N'))
$reg = 'HKCU:\Software\LatchVdd-' + [guid]::NewGuid().ToString('N')
$programFiles = $env:ProgramFiles
try {
    New-Item -ItemType Directory -Path "$fixture\original", "$fixture\Latch" -Force | Out-Null
    New-Item -Path $reg -Force | Out-Null
    $env:ProgramFiles = $fixture
    $source = $source.Replace('C:\VirtualDisplayDriver', "$fixture\original").Replace('HKLM:\SOFTWARE\MikeTheTech\VirtualDisplayDriver', $reg).Replace('HKLM:\SOFTWARE\Latch\Display', "$reg\Record")
    function Step($label) { Write-Output $label }
    function Assert-ProtectedTree($path) { if ($path -eq "$fixture\original") { throw 'untrusted original folder' } }
    function Assert-Protected($path) {}
    function Start-Sleep { param($Milliseconds) }
    function pnputil {
        $script:restarts++
        $global:LASTEXITCODE = 0
        if ($script:failRestart -and $script:restarts -eq 1) { $global:LASTEXITCODE = 1 }
    }
    function Get-PnpDevice {
        param($Class, $ErrorAction)
        if ($Class -eq 'Display') { return [pscustomobject]@{FriendlyName='Virtual Display Driver';Status='OK';InstanceId='fixture'} }
        if (-not $script:reject -or $script:restarts -ne 1) { return [pscustomobject]@{InstanceId='DISPLAY\MTT1337\fixture';Status='OK'} }
    }
    $original = '<vdd_settings><monitors><count>1</count></monitors><resolutions><resolution><width>1920</width><height>1080</height><refresh_rate>60</refresh_rate><refresh_rate>120</refresh_rate></resolution><resolution><width>2560</width><height>1440</height><refresh_rate>60</refresh_rate><refresh_rate>120</refresh_rate></resolution></resolutions><custom>preserved</custom></vdd_settings>'
    foreach ($case in 'accept', 'reject', 'restart-fails', 'invalid-xml') {
        $script:restarts = 0
        $script:reject = $case -eq 'reject'
        $script:failRestart = $case -eq 'restart-fails'
        $setupFailed = $false
        $inputXml = if ($case -eq 'invalid-xml') { '<broken' } else { $original }
        [IO.File]::WriteAllText("$fixture\original\vdd_settings.xml", $inputXml)
        # A failed older setup can leave both this empty folder and record.
        New-Item -ItemType Directory "$fixture\Latch\Display" -Force | Out-Null
        New-Item "$reg\Record" -Force | Out-Null
        New-ItemProperty "$reg\Record" PreviousVddPath -Value absent -Force | Out-Null
        $output = . ([scriptblock]::Create($source)) *>&1 | Out-String
        Write-Output "$case`n$output"
        if ($setupFailed) { throw "$case failed setup" }
        if ($vddModesUnchanged -ne ($case -ne 'accept')) { throw 'incorrect setup summary status' }
        if ([IO.File]::ReadAllText("$fixture\original\vdd_settings.xml") -cne $inputXml) { throw 'original XML changed' }
        if ($case -eq 'accept') {
            [xml]$xml = Get-Content "$fixture\Latch\Display\vdd_settings.xml" -Raw
            $sizes = @($xml.vdd_settings.resolutions.resolution | ForEach-Object { "$($_.width)x$($_.height)" })
            if (@($sizes | Sort-Object -Unique).Count -ne $sizes.Count) { throw 'duplicate resolutions' }
            foreach ($r in $xml.vdd_settings.resolutions.resolution) {
                $expected = if ("$($r.width)x$($r.height)" -in @('1920x1080','2560x1440')) { 7 } else { 2 }
                if (@($r.refresh_rate).Count -ne $expected) { throw 'missing refresh rates' }
            }
            if ($xml.vdd_settings.custom -ne 'preserved') { throw 'custom XML lost' }
            if ($restarts -ne 1) { throw 'expected one restart' }
            # Idempotence uses the relocated file and does not restart.
            & ([scriptblock]::Create($source)) | Out-Host
            if ($restarts -ne 1) { throw 'second run restarted' }
            Remove-ItemProperty $reg VDDPATH
        } else {
            if (-not $output.Contains('WARNING: virtual display modes unchanged')) { throw 'missing warning' }
            if ($case -ne 'invalid-xml') {
                if ($restarts -ne 2) { throw 'rollback did not restart' }
                if ((Get-ItemProperty $reg).VDDPATH) { throw 'VDDPATH not restored' }
                if (Test-Path "$fixture\Latch\Display") { throw 'stale relocation folder remains' }
                if (Test-Path "$reg\Record") { throw 'stale relocation record remains' }
            }
        }
        Remove-Item "$fixture\Latch\Display" -Recurse -Force -ErrorAction SilentlyContinue
        Remove-Item "$reg\Record" -Recurse -Force -ErrorAction SilentlyContinue
    }
    'virtual display setup checks passed'
} finally {
    $env:ProgramFiles = $programFiles
    Remove-Item $fixture -Recurse -Force -ErrorAction SilentlyContinue
    Remove-Item $reg -Recurse -Force -ErrorAction SilentlyContinue
}
