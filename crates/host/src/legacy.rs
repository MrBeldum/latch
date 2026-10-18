//! What this host registered with the operating system before 4.1.0, when
//! Latch was called BroLink, and how an upgraded copy retires it.
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

/// What a 4.0 setup left on a Windows PC, as names the retire step looks
/// for. Only [`retire_windows_ps`] uses the real ones; the Windows CI job
/// runs the same step on temporary folders and a dummy service.
pub struct Retiring<'a> {
    pub engine_dir: &'a str,
    pub service: &'a str,
    pub task: &'a str,
    pub rules: &'a [&'a str],
}

/// PowerShell for the elevated setup script, run before it looks for an
/// engine: removes the old logon task and firewall rules, stops and
/// deletes the old service, and moves the engine's folder, with the pairing
/// state in its `config`, to `engine_dir`. Safe on a PC with none of it.
pub fn retire_windows_ps(engine_dir: &str) -> String {
    retire_windows_ps_with(
        &Retiring {
            engine_dir: WINDOWS_ENGINE_DIR,
            service: WINDOWS_SERVICE,
            task: WINDOWS_ENGINE_TASK,
            rules: &WINDOWS_RULES,
        },
        engine_dir,
    )
}

/// [`retire_windows_ps`] for any old layout.
///
/// The move is retried: an engine restarted in the gap, an antivirus scan
/// or an Explorer window holds the folder for a moment. If it still fails,
/// the old engine stays where it is and `$retiredState` names its `config`,
/// which [`restore_retired_state_ps`] copies into the fresh engine setup
/// then installs, so paired devices carry over either way. Nothing here
/// throws: the old firewall rules are already gone, and setup has to go on
/// to open the new ones.
pub fn retire_windows_ps_with(old: &Retiring<'_>, engine_dir: &str) -> String {
    let q = |s: &str| s.replace('\'', "''");
    let rules = old
        .rules
        .iter()
        .map(|r| format!("'{}'", q(r)))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        r#"Step "Retiring what an earlier version registered"
$retiredState = $null
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
    Get-CimInstance Win32_Process -Filter "Name='sunshine.exe'" -ErrorAction SilentlyContinue | Where-Object {{ $_.ExecutablePath -like ($oldEngine + '\*') }} | ForEach-Object {{ Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }}
    if (Get-Service -Name '{service}' -ErrorAction SilentlyContinue) {{ sc.exe delete '{service}' | Out-Null }}
    if (-not (Test-Path -LiteralPath (Join-Path '{engine}' 'sunshine.exe'))) {{
        Step "Moving the streaming engine to {engine}"
        New-Item -ItemType Directory -Force -Path (Split-Path '{engine}') | Out-Null
        # Moved onto a folder that exists, the old one would land INSIDE it
        # (engine\engine). Such a folder holds no engine, only what an
        # earlier run left part way: an empty one goes, anything else is
        # kept aside.
        if (Test-Path -LiteralPath '{engine}') {{
            if (Get-ChildItem -LiteralPath '{engine}' -Force -ErrorAction SilentlyContinue | Select-Object -First 1) {{
                $aside = '{engine}.partial-' + (Get-Date -Format yyyyMMddHHmmss)
                Step "  keeping what was in the way as $aside"
                Move-Item -LiteralPath '{engine}' -Destination $aside -ErrorAction SilentlyContinue
            }} else {{
                Remove-Item -LiteralPath '{engine}' -Force -ErrorAction SilentlyContinue
            }}
        }}
        $moved = $false
        if (-not (Test-Path -LiteralPath '{engine}')) {{
            foreach ($try in 1..5) {{
                # Again each time: one may have been started from there since.
                Get-CimInstance Win32_Process -Filter "Name='sunshine.exe'" -ErrorAction SilentlyContinue | Where-Object {{ $_.ExecutablePath -like ($oldEngine + '\*') }} | ForEach-Object {{ Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }}
                Start-Sleep -Seconds ([Math]::Min($try, 2))
                try {{
                    Move-Item -LiteralPath $oldEngine -Destination '{engine}' -ErrorAction Stop
                    $moved = $true
                    break
                }} catch {{ Write-Output "  could not move the engine (try $try of 5): $_" }}
            }}
        }}
        if (-not $moved) {{
            $retiredState = Join-Path $oldEngine 'config'
            Step "  the earlier engine stays in $oldEngine; setup installs a new one and copies its pairing state across"
        }}
    }}
    $oldRoot = Split-Path $oldEngine
    if (-not (Get-ChildItem -LiteralPath $oldRoot -Force -ErrorAction SilentlyContinue)) {{
        Remove-Item -LiteralPath $oldRoot -Force -ErrorAction SilentlyContinue
    }}
}}
"#,
        task = q(old.task),
        rules = rules,
        old = q(old.engine_dir),
        service = q(old.service),
        engine = q(engine_dir),
    )
}

/// PowerShell for setup's install step, once a fresh engine is in
/// `engine_dir`: when the retire step could not move the old engine, copy
/// its pairing state (the server's identity and paired devices, and the
/// apps and settings) into the new one, which has none yet. A failure is
/// logged, never thrown, because the new engine streams either way; its
/// devices then pair again.
pub fn restore_retired_state_ps(engine_dir: &str) -> String {
    let list = |files: &[&[&str]]| {
        files
            .iter()
            .map(|f| format!("'{}'", f.join(r"\")))
            .collect::<Vec<_>>()
            .join(", ")
    };
    format!(
        r#"if ($retiredState -and -not (Test-Path -LiteralPath (Join-Path '{engine}' 'config\sunshine_state.json'))) {{
        Step "Copying the pairing state from $retiredState"
        try {{
            $to = Join-Path '{engine}' 'config'
            New-Item -ItemType Directory -Force -Path (Join-Path $to 'credentials') | Out-Null
            foreach ($f in @({required}, {optional})) {{
                $from = Join-Path $retiredState $f
                if (Test-Path -LiteralPath $from) {{ Copy-Item -LiteralPath $from -Destination (Join-Path $to $f) -Force -ErrorAction Stop }}
            }}
            foreach ($f in @({required})) {{
                if (-not (Test-Path -LiteralPath (Join-Path $to $f))) {{ Write-Output "  $f was not there to copy; paired devices pair again" }}
            }}
        }} catch {{ Write-Output "  could not copy the pairing state ($_); paired devices pair again" }}
    }}
    "#,
        engine = engine_dir.replace('\'', "''"),
        required = list(crate::migrate::REQUIRED),
        optional = list(crate::migrate::OPTIONAL),
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

    /// The fragment with its comments stripped, for "never does" checks.
    fn code(s: &str) -> String {
        s.lines()
            .filter(|l| !l.trim_start().starts_with('#'))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn a_failed_move_is_retried_then_leaves_the_pairing_state_for_the_new_engine() {
        let s = code(&retire_windows_ps(r"C:\Program Files\Latch\engine"));
        let reset = s.find("$retiredState = $null").expect("reset first");
        let retry = s.find("foreach ($try in 1..5)").expect("retries");
        let kill = s[retry..].find("Stop-Process").expect("kill in the loop") + retry;
        let mov = s.find("Move-Item -LiteralPath $oldEngine").expect("move");
        let kept = s
            .find("$retiredState = Join-Path $oldEngine 'config'")
            .expect("state kept for the fresh engine");
        assert!(
            reset < retry && retry < kill && kill < mov && mov < kept,
            "{s}"
        );
        // A throw would end setup before the new firewall rules, with the
        // old ones already deleted: a remote PC would drop off the tailnet.
        assert!(!s.contains("throw"), "{s}");
    }

    #[test]
    fn a_folder_in_the_way_is_cleared_before_the_move_so_nothing_nests() {
        let s = code(&retire_windows_ps(r"C:\Program Files\Latch\engine"));
        let aside = s
            .find(r"$aside = 'C:\Program Files\Latch\engine.partial-'")
            .expect("a partial folder is kept aside");
        let empty = s
            .find(r"Remove-Item -LiteralPath 'C:\Program Files\Latch\engine' -Force")
            .expect("an empty one is removed");
        let guard = s
            .find(r"if (-not (Test-Path -LiteralPath 'C:\Program Files\Latch\engine'))")
            .expect("no move onto a folder that is still there");
        let mov = s.find("Move-Item -LiteralPath $oldEngine").expect("move");
        assert!(aside < guard && empty < guard && guard < mov, "{s}");
    }

    #[test]
    fn the_state_copy_runs_only_after_a_failed_move_and_never_throws() {
        let s = restore_retired_state_ps(r"C:\Program Files\Latch\engine");
        assert!(
            s.starts_with(
                r"if ($retiredState -and -not (Test-Path -LiteralPath (Join-Path 'C:\Program Files\Latch\engine' 'config\sunshine_state.json')))"
            ),
            "{s}"
        );
        for f in crate::migrate::REQUIRED
            .iter()
            .chain(crate::migrate::OPTIONAL)
        {
            assert!(s.contains(&format!("'{}'", f.join(r"\"))), "{f:?}: {s}");
        }
        assert!(s.contains("Copy-Item -LiteralPath $from"), "{s}");
        assert!(s.contains("} catch {"), "{s}");
        assert!(!code(&s).contains("throw"), "{s}");
    }

    #[test]
    fn names_from_elsewhere_are_quoted_as_literals() {
        let s = retire_windows_ps_with(
            &Retiring {
                engine_dir: r"D:\a\_temp\O'Brien\BroLink\engine",
                service: "LatchRetireSelftest",
                task: "LatchRetireSelftestTask",
                rules: &["Latch 'Retire' Selftest"],
            },
            r"D:\a\_temp\O'Brien\Latch\engine",
        );
        assert!(
            s.contains(r"$oldEngine = 'D:\a\_temp\O''Brien\BroLink\engine'"),
            "{s}"
        );
        assert!(s.contains("'Latch ''Retire'' Selftest'"), "{s}");
        assert!(s.contains("sc.exe delete 'LatchRetireSelftest'"), "{s}");
        assert!(!s.contains(WINDOWS_ENGINE_DIR), "{s}");
        assert!(!s.contains(WINDOWS_SERVICE), "{s}");
    }

    /// The seam the Windows CI job runs the retire step through (see
    /// `.github/workflows/ci.yml`): on temporary folders and a dummy service,
    /// never the real ones, which a run of this repo's tests on a real PC
    /// must not touch. `LATCH_DUMP_RETIRE_SCRIPT` writes the script; the
    /// `LATCH_DUMP_RETIRE_OLD`, `_NEW` and `_SERVICE` it uses are required.
    #[test]
    fn the_retire_step_can_be_dumped_for_a_windows_run() {
        let Ok(out) = std::env::var("LATCH_DUMP_RETIRE_SCRIPT") else {
            return;
        };
        let var = |k: &str| std::env::var(k).unwrap_or_else(|_| panic!("{k} is required"));
        let (old, new, service) = (
            var("LATCH_DUMP_RETIRE_OLD"),
            var("LATCH_DUMP_RETIRE_NEW"),
            var("LATCH_DUMP_RETIRE_SERVICE"),
        );
        assert!(
            !old.eq_ignore_ascii_case(WINDOWS_ENGINE_DIR)
                && !service.eq_ignore_ascii_case(WINDOWS_SERVICE),
            "the selftest never runs on the real engine"
        );
        let retire = retire_windows_ps_with(
            &Retiring {
                engine_dir: &old,
                service: &service,
                task: "LatchRetireSelftestTask",
                rules: &["Latch Retire Selftest"],
            },
            &new,
        );
        let script = format!(
            "$ErrorActionPreference = 'Continue'\nfunction Step($m) {{ Write-Output $m }}\n{retire}{}\nexit 0\n",
            restore_retired_state_ps(&new)
        );
        std::fs::write(out, script).expect("write the script");
    }

    #[test]
    fn an_old_engine_program_is_still_recognised() {
        assert!(ENGINE_EXE_NAMES.contains(&"BroLinkStreaming"));
        assert!(ENGINE_EXE_NAMES.contains(&"brolink-engine"));
    }
}
