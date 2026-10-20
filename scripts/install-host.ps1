# Install Latch Host for the current user: copy the exe and the bundled
# engine archive into %LOCALAPPDATA%\Latch, add Start Menu and Desktop
# shortcuts, and open it. The app itself runs the one administrator step
# (engine, firewall, Wake-on-LAN) when you click "Set up this PC".
#
# -NoStart leaves the window closed. The background service this stops to
# replace the exe always comes back, in the signed-in user's session: run
# over SSH (session 0, no desktop) it is started there through a one-off
# scheduled task, since a copy started in the SSH session could not see the
# desktop and ends with it.
[CmdletBinding()]
param(
    [string]$Exe = "",
    [switch]$NoStart
)

$ErrorActionPreference = "Stop"
if (-not $Exe) {
    # Beside this script in the release zip; in the repo, under target\release.
    $Here = Split-Path -Parent $MyInvocation.MyCommand.Path
    $Root = Split-Path -Parent $Here
    foreach ($c in @(
            (Join-Path $Here "latch-host.exe"),
            (Join-Path $Root "latch-host.exe"),
            (Join-Path $Root "target\release\latch-host.exe")
        )) {
        if (Test-Path $c) { $Exe = $c; break }
    }
}
if (-not $Exe -or -not (Test-Path $Exe)) {
    Write-Error "latch-host.exe not found. Download a release or build with: cargo build --release -p latch-host"
}

$DestDir = Join-Path $env:LOCALAPPDATA "Latch"
# Where versions before 4.1 installed. Their
# settings, pairing identity and engine archive come across, and the folder
# goes once this one is in place.
$OldDir = Join-Path $env:LOCALAPPDATA "BroLink"
$DestExe = Join-Path $DestDir "latch-host.exe"

# A running service or panel holds the old exe open; stop them first.
try {
    Invoke-RestMethod -Method Post -Uri "http://127.0.0.1:47850/v1/quit" -ContentType 'application/json' -Body '{}' -TimeoutSec 2 | Out-Null
    Start-Sleep -Milliseconds 800
} catch {}
Get-Process latch-host, brolink-host -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Milliseconds 300
if ((Test-Path $OldDir) -and -not (Test-Path (Join-Path $DestDir "host.toml"))) {
    New-Item -ItemType Directory -Force -Path $DestDir | Out-Null
    Get-ChildItem -LiteralPath $OldDir -Force | Where-Object { $_.Name -notmatch '\.exe(\.old|\.new)?$' } |
        ForEach-Object { Copy-Item -LiteralPath $_.FullName -Destination $DestDir -Recurse -Force }
    Write-Host "Settings carried over from the previous install."
}
New-Item -ItemType Directory -Force -Path $DestDir | Out-Null
Copy-Item $Exe $DestExe -Force
Write-Host "Installed: $DestExe"
# Same filename setup.rs ENGINE_ZIP looks for beside the exe.
$Zip = Join-Path (Split-Path -Parent $Exe) "Sunshine-Windows-AMD64-lite.zip"
if (Test-Path $Zip) {
    Copy-Item $Zip $DestDir -Force
    Write-Host "Bundled engine archive copied; setup will not need to download it."
}

$Wsh = New-Object -ComObject WScript.Shell
$Desktop = [Environment]::GetFolderPath("Desktop")
$StartMenu = Join-Path ([Environment]::GetFolderPath("StartMenu")) "Programs"
New-Item -ItemType Directory -Force -Path $StartMenu | Out-Null
foreach ($folder in @($Desktop, $StartMenu)) {
    Remove-Item -LiteralPath (Join-Path $folder "BroLink Host.lnk") -Force -ErrorAction SilentlyContinue
    $Lnk = $Wsh.CreateShortcut((Join-Path $folder "Latch Host.lnk"))
    $Lnk.TargetPath = $DestExe
    $Lnk.WorkingDirectory = $DestDir
    $Lnk.Description = "Your PC, from your Mac"
    $Lnk.Save()
}

# The background service at logon; the app offers the same toggle.
$Run = "HKCU:\Software\Microsoft\Windows\CurrentVersion\Run"
Remove-ItemProperty -Path $Run -Name "BroLinkHost" -ErrorAction SilentlyContinue
New-ItemProperty -Path $Run -Name "LatchHost" -Value "`"$DestExe`" --background" -PropertyType String -Force | Out-Null

if (Test-Path $OldDir) {
    Remove-Item -LiteralPath $OldDir -Recurse -Force -ErrorAction SilentlyContinue
    if (Test-Path $OldDir) { Write-Host "Could not remove the previous install folder completely; it is safe to delete." }
}

Write-Host "Shortcut: Desktop\Latch Host.lnk"
Write-Host "Open Latch Host and click 'Set up this PC' once."
Write-Host "If you already ran setup from the unzipped copy, run it once more from this install so the firewall allows $DestExe."
if ((Get-Process -Id $PID).SessionId -eq 0) {
    # The task runs as this user, unelevated like the Run key, in the session
    # they are signed in to. Task Scheduler's defaults would stop the host
    # after 72 hours, or on battery, and run it at below-normal priority.
    $TaskName = "LatchHostStart"
    $up = $false
    try {
        $action = New-ScheduledTaskAction -Execute $DestExe -Argument "--background" -WorkingDirectory $DestDir
        $principal = New-ScheduledTaskPrincipal -UserId "$env:USERDOMAIN\$env:USERNAME" -LogonType Interactive -RunLevel Limited
        $settings = New-ScheduledTaskSettingsSet -ExecutionTimeLimit ([TimeSpan]::Zero) -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -Priority 5
        Register-ScheduledTask -TaskName $TaskName -Action $action -Principal $principal -Settings $settings -Force | Out-Null
        Start-ScheduledTask -TaskName $TaskName
        foreach ($i in 1..20) {
            Start-Sleep -Milliseconds 750
            try {
                Invoke-RestMethod -Uri "http://127.0.0.1:47850/v1/status" -UseBasicParsing -TimeoutSec 2 | Out-Null
                $up = $true
                break
            } catch {}
        }
    } catch {
        Write-Host "Could not start Latch Host in the signed-in session: $_"
    }
    # The task has no trigger, so left registered it never runs by itself;
    # removing it could end the copy it just started.
    if ($up) {
        Write-Host "Latch Host is running in the signed-in user's session."
    } else {
        Write-Host "Nobody is signed in to this PC's desktop: Latch Host starts at the next sign-in."
    }
} elseif ($NoStart) {
    Start-Process -FilePath $DestExe -ArgumentList "--background" -WorkingDirectory $DestDir
} else {
    Start-Process -FilePath $DestExe -WorkingDirectory $DestDir
}
