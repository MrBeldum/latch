//! What this host registered with the operating system before 4.1.0,
//! and how an upgraded copy retires it.
//!
//! An upgrade by the updater swaps the program and nothing else, so the
//! launch agent, the user unit, the Windows service, the scheduled task,
//! the firewall rules and the engine's folder all still carry the old
//! name. Every one is recognised here and replaced by its new name when
//! the new copy first runs or when setup next does. Until then the old
//! registrations keep pointing at things that still exist, so a host that
//! was updated and never set up again keeps working.

/// What an engine that an old setup branded calls itself.
pub const ENGINE_DESCRIPTION: &str = "BroLink Streaming";
/// The engine's renamed program as an old setup left it on disk: the Mac's
/// app binary, and the Linux package's.
pub const ENGINE_EXE_NAMES: [&str; 2] = ["BroLinkStreaming", "brolink-engine"];

/// The folder a 4.0 Windows setup unpacked the engine into.
pub const WINDOWS_ENGINE_DIR: &str = r"C:\Program Files\BroLink\engine";
pub const WINDOWS_SERVICE: &str = "BroLinkStream";
/// The logon task that runs the engine as the signed-in user.
pub const WINDOWS_ENGINE_TASK: &str = "BroLinkEngineUser";
/// The value under `HKCU\...\Run` that starts the host at logon.
pub const WINDOWS_RUN_VALUE: &str = "BroLinkHost";
/// Firewall rules a 4.0 setup opened, by name.
pub const WINDOWS_RULES: [&str; 6] = [
    "BroLink Host",
    "BroLink wake",
    "BroLink Streaming TCP",
    "BroLink Streaming UDP",
    "BroLink Sunshine TCP",
    "BroLink Sunshine UDP",
];

/// The Mac app's folder and its launch agent's label.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub const MAC_BUNDLE: &str = "BroLink.app";
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub const MAC_LAUNCH_AGENT: &str = "dev.brolink.node";
/// The Linux user unit.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub const LINUX_USER_UNIT: &str = "brolink.service";

/// PowerShell for the elevated setup script, run before it looks for an
/// engine: removes the old logon task and firewall rules, stops and
/// deletes the old service, and moves the engine's folder, with the pairing
/// state in its `config`, to `engine_dir`. Safe on a PC with none of it.
pub fn retire_windows_ps(engine_dir: &str) -> String {
    let rules = WINDOWS_RULES
        .iter()
        .map(|r| format!("'{r}'"))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        r#"Step "Retiring what an earlier version registered"
foreach ($t in '{task}') {{
    if (Get-ScheduledTask -TaskName $t -ErrorAction SilentlyContinue) {{
        Stop-ScheduledTask -TaskName $t -ErrorAction SilentlyContinue
        Unregister-ScheduledTask -TaskName $t -Confirm:$false -ErrorAction SilentlyContinue
    }}
}}
foreach ($r in {rules}) {{ netsh advfirewall firewall delete rule name="$r" | Out-Null }}
$oldEngine = '{old}'
if (Test-Path -LiteralPath (Join-Path $oldEngine 'sunshine.exe')) {{
    Stop-Service -Name '{service}' -Force -ErrorAction SilentlyContinue
    Get-CimInstance Win32_Process -Filter "Name='sunshine.exe'" | Where-Object {{ $_.ExecutablePath -like ($oldEngine + '\*') }} | ForEach-Object {{ Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }}
    if (Get-Service -Name '{service}' -ErrorAction SilentlyContinue) {{ sc.exe delete '{service}' | Out-Null }}
    if (-not (Test-Path -LiteralPath (Join-Path '{engine}' 'sunshine.exe'))) {{
        Step "Moving the streaming engine to {engine}"
        try {{
            New-Item -ItemType Directory -Force -Path (Split-Path '{engine}') | Out-Null
            Start-Sleep -Seconds 1
            Move-Item -LiteralPath $oldEngine -Destination '{engine}' -ErrorAction Stop
        }} catch {{ Write-Output "  could not move the engine: $_" }}
    }}
    $oldRoot = Split-Path $oldEngine
    if (-not (Get-ChildItem -LiteralPath $oldRoot -Force -ErrorAction SilentlyContinue)) {{
        Remove-Item -LiteralPath $oldRoot -Force -ErrorAction SilentlyContinue
    }}
}}
"#,
        task = WINDOWS_ENGINE_TASK,
        rules = rules,
        old = WINDOWS_ENGINE_DIR,
        service = WINDOWS_SERVICE,
        engine = engine_dir,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_windows_fragment_removes_every_old_registration_and_moves_the_engine() {
        let s = retire_windows_ps(r"C:\Program Files\Latch\engine");
        for rule in WINDOWS_RULES {
            assert!(s.contains(&format!("'{rule}'")), "{rule}: {s}");
        }
        assert!(s.contains("Unregister-ScheduledTask"), "{s}");
        assert!(s.contains("sc.exe delete 'BroLinkStream'"), "{s}");
        assert!(
            s.contains(
                r"Move-Item -LiteralPath $oldEngine -Destination 'C:\Program Files\Latch\engine'"
            ),
            "{s}"
        );
        // The old folder is only emptied after it has been moved from, and
        // an engine already in the new place is never overwritten.
        assert!(s.contains(r"-not (Test-Path -LiteralPath (Join-Path 'C:\Program Files\Latch\engine' 'sunshine.exe'))"));
        assert!(!s.contains("Remove-Item -Recurse"), "{s}");
    }

    #[test]
    fn an_old_engine_program_is_still_recognised() {
        assert!(ENGINE_EXE_NAMES.contains(&"BroLinkStreaming"));
        assert!(ENGINE_EXE_NAMES.contains(&"brolink-engine"));
    }
}
