# Run the streaming engine as the logged-on user and put an active playback
# device back as the Windows default. The engine's own startup clears the
# default when Steam Streaming Speakers are the only active device.
$ErrorActionPreference = "Continue"
# The engine's folder is Latch's, or the one an earlier version used until
# setup moves it; the service is named the same way. Either may be there.
$engineDir = @("C:\Program Files\Latch\engine", "C:\Program Files\BroLink\engine") |
    Where-Object { Test-Path (Join-Path $_ "sunshine.exe") } | Select-Object -First 1
if (-not $engineDir) { Write-Output "NO_ENGINE"; exit 0 }
$exe = Join-Path $engineDir "sunshine.exe"
$conf = Join-Path $engineDir "config\sunshine.conf"

function Get-ActiveRender {
    $root = "HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\MMDevices\Audio\Render"
    if (-not (Test-Path $root)) { return @() }
    Get-ChildItem $root | ForEach-Object {
        $state = [int](Get-ItemProperty $_.PSPath).DeviceState
        if (($state -band 1) -eq 0) { return }
        $friendly = $null
        $desc = $null
        try { $friendly = (Get-ItemProperty -LiteralPath "$($_.PSPath)\Properties" -Name "{a45c254e-df1c-4efd-8020-67d146a850e0},2")."{a45c254e-df1c-4efd-8020-67d146a850e0},2" } catch {}
        try { $desc = (Get-ItemProperty -LiteralPath "$($_.PSPath)\Properties" -Name "{b3f8fa53-0004-438e-9003-51a46e139bfc},6")."{b3f8fa53-0004-438e-9003-51a46e139bfc},6" } catch {}
        [pscustomobject]@{
            Id       = "{0.0.0.00000000}." + $_.PSChildName
            Friendly = $friendly
            Desc     = $desc
        }
    }
}

# Session 0 has no desktop. An SSH or WinRM shell runs there, and an engine
# started from it could neither capture the screen nor take input, so
# nothing is stopped or started from there. Setup run that way hands the
# engine to the signed-in user's logon task instead.
$mySession = [System.Diagnostics.Process]::GetCurrentProcess().SessionId
if ($mySession -eq 0) { Write-Output "NOT_INTERACTIVE"; exit 2 }

function Get-EngineProcesses {
    Get-CimInstance -ClassName Win32_Process -Filter "Name='sunshine.exe'" | ForEach-Object {
        $o = Invoke-CimMethod -InputObject $_ -MethodName GetOwner
        [pscustomobject]@{
            Pid     = $_.ProcessId
            Session = $_.SessionId
            User    = "$($o.Domain)\$($o.User)"
        }
    }
}

$services = "LatchStream", "BroLinkStream"
$me = "$env:USERDOMAIN\$env:USERNAME"

# Ours is an engine this user runs in this session. Any other is foreign:
# the service's SYSTEM copy, one left in session 0 by a setup run over SSH,
# another user's, or one whose owner cannot be read. Callers wrap the list
# in @(): Windows PowerShell unrolls a one-item list into a lone object,
# which has no Count.
function Get-ForeignEngines {
    Get-EngineProcesses | Where-Object { $_.User -ine $me -or $_.Session -ne $mySession }
}

# A service that is Running counts even before its engine shows up: the
# service wrapper starts the engine a few seconds after it reports Running.
function Test-ServiceUp {
    @($services | Where-Object { (Get-Service -Name $_ -ErrorAction SilentlyContinue).Status -eq 'Running' }).Count -gt 0
}

# Every engine is looked at, not just the first listed: an older user engine
# listed first would otherwise hide a SYSTEM one the service started beside
# it. $LatchForce is set by setup, which has just rewritten the engine's
# login and settings, so even an engine of ours starts again to read them.
$all = @(Get-EngineProcesses)
$foreign = @(Get-ForeignEngines)
if ($all.Count -eq 0 -or $foreign.Count -gt 0 -or (Test-ServiceUp) -or $LatchForce) {
    foreach ($service in $services) {
        Set-Service $service -StartupType Manual -ErrorAction SilentlyContinue
        Stop-Service $service -Force -ErrorAction SilentlyContinue
    }
    foreach ($p in $foreign) { Stop-Process -Id $p.Pid -Force -ErrorAction SilentlyContinue }
    Start-Sleep -Seconds 2
    # Without administrator rights (the logon task, Latch Host itself) the
    # service and a SYSTEM engine cannot be stopped. The user's own engine
    # is then left running rather than killed for nothing.
    $left = @(Get-ForeignEngines)
    if ($left.Count -gt 0) {
        Write-Output ("STILL_RUNNING {0} pid={1}" -f $left[0].User, $left[0].Pid)
        exit 1
    }
    if (Test-ServiceUp) {
        Write-Output "STILL_RUNNING service"
        exit 1
    }
    $own = @(Get-EngineProcesses)
    if ($own.Count -gt 0) {
        foreach ($p in $own) { Stop-Process -Id $p.Pid -Force -ErrorAction SilentlyContinue }
        Start-Sleep -Seconds 1
        $left = @(Get-EngineProcesses)
        if ($left.Count -gt 0) {
            Write-Output ("STILL_RUNNING {0} pid={1}" -f $left[0].User, $left[0].Pid)
            exit 1
        }
    }
    # The engine is a console program. Started plainly it owns a console
    # window on the desktop being streamed, and closing that window ends
    # the engine and with it the stream. SW_HIDE at creation keeps the
    # console from ever showing.
    $startup = New-CimInstance -ClassName Win32_ProcessStartup -ClientOnly -Property @{ ShowWindow = [UInt16]0 }
    $created = Invoke-CimMethod -ClassName Win32_Process -MethodName Create -Arguments @{
        CommandLine               = "`"$exe`" `"$conf`""
        CurrentDirectory          = $engineDir
        ProcessStartupInformation = $startup
    }
    if ($created.ReturnValue -ne 0) {
        Write-Output ("CREATE_FAILED {0}" -f $created.ReturnValue)
        exit 1
    }
    # platf::init disables Steam Speakers when they are the default; wait
    # until that has run before putting a default back.
    Start-Sleep -Seconds 6
}

$devices = @(Get-ActiveRender)
if ($devices.Count -eq 0) { Write-Output "NO_ACTIVE_RENDER"; exit 0 }
# Compiled only now: C# goes through csc, about a second of CPU, and a PC
# with no playback device at all never needs it.
Add-Type -TypeDefinition @"
using System;
using System.Runtime.InteropServices;
[ComImport, Guid("870AF99C-171D-4F9E-AF0D-E63DF40C2BC9")]
internal class PolicyConfigClient {}
[Guid("F8679F50-850A-41CF-9C72-430F290290C8"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
internal interface IPolicyConfig {
    [PreserveSig] int GetMixFormat(string pszDeviceName, IntPtr ppFormat);
    [PreserveSig] int GetDeviceFormat(string pszDeviceName, bool bDefault, IntPtr ppFormat);
    [PreserveSig] int ResetDeviceFormat(string pszDeviceName);
    [PreserveSig] int SetDeviceFormat(string pszDeviceName, IntPtr pEndpointFormat, IntPtr MixFormat);
    [PreserveSig] int GetProcessingPeriod(string pszDeviceName, bool bDefault, IntPtr pmftDefaultPeriod, IntPtr pmftMinimumPeriod);
    [PreserveSig] int SetProcessingPeriod(string pszDeviceName, IntPtr pmftPeriod);
    [PreserveSig] int GetShareMode(string pszDeviceName, IntPtr pMode);
    [PreserveSig] int SetShareMode(string pszDeviceName, IntPtr mode);
    [PreserveSig] int GetPropertyValue(string pszDeviceName, bool bFxStore, IntPtr key, IntPtr pv);
    [PreserveSig] int SetPropertyValue(string pszDeviceName, bool bFxStore, IntPtr key, IntPtr pv);
    [PreserveSig] int SetDefaultEndpoint(string pszDeviceName, int eRole);
    [PreserveSig] int SetEndpointVisibility(string pszDeviceName, bool bVisible);
}
public static class LatchAudio {
    public static int SetDefault(string id) {
        var cfg = (IPolicyConfig)new PolicyConfigClient();
        int hr = 0;
        for (int role = 0; role <= 2; role++) {
            hr = cfg.SetDefaultEndpoint(id, role);
            if (hr < 0) return hr;
        }
        return 0;
    }
}
"@
$pick = $devices | Where-Object { $_.Desc -match "Steam Streaming" -or $_.Friendly -match "Steam Streaming" } | Select-Object -First 1
if (-not $pick) { $pick = $devices | Select-Object -First 1 }
$hr = [LatchAudio]::SetDefault($pick.Id)
Write-Output ("OK {0} hr=0x{1:X8}" -f $pick.Id, $hr)
