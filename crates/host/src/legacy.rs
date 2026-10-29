//! Retire registrations from earlier hosts; keep the host's data migration.
pub const WINDOWS_RUN_VALUE: &str = "BroLinkHost";
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub const MAC_BUNDLE: &str = "BroLink.app";
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub const MAC_LAUNCH_AGENT: &str = "dev.brolink.node";
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub const LINUX_USER_UNIT: &str = "brolink.service";

pub fn retire_windows_ps() -> String {
    retire_windows_ps_with(
        &[
            r"C:\Program Files\Latch\engine",
            r"C:\Program Files\BroLink\engine",
        ],
        &["LatchStream", "BroLinkStream"],
        &["LatchEngineUser", "BroLinkEngineUser"],
        &[
            "Latch Streaming TCP",
            "Latch Streaming UDP",
            "Latch Sunshine TCP",
            "Latch Sunshine UDP",
            "BroLink Host",
            "BroLink wake",
            "BroLink Streaming TCP",
            "BroLink Streaming UDP",
            "BroLink Sunshine TCP",
            "BroLink Sunshine UDP",
        ],
    )
}

fn retire_windows_ps_with(
    dirs: &[&str],
    services: &[&str],
    tasks: &[&str],
    rules: &[&str],
) -> String {
    let list = |items: &[&str]| {
        items
            .iter()
            .map(|s| format!("'{}'", s.replace('\'', "''")))
            .collect::<Vec<_>>()
            .join(", ")
    };
    format!(
        r#"Step "Removing the previous streaming service"
foreach ($task in @({tasks})) {{
    if (Get-ScheduledTask -TaskName $task -ErrorAction SilentlyContinue) {{
        Stop-ScheduledTask -TaskName $task -ErrorAction SilentlyContinue
        Unregister-ScheduledTask -TaskName $task -Confirm:$false -ErrorAction SilentlyContinue
    }}
}}
foreach ($service in @({services})) {{
    if (Get-Service -Name $service -ErrorAction SilentlyContinue) {{
        try {{ Stop-Service -Name $service -Force -ErrorAction Stop }} catch {{ Write-Output "  stop $service : $_"; $setupFailed = $true }}
        sc.exe delete $service | Out-Null
        if ($LASTEXITCODE -notin 0, 1060, 1072) {{ $setupFailed = $true }}
    }}
}}
$sunshine = Get-CimInstance Win32_Service -Filter "Name = 'SunshineService'" -ErrorAction SilentlyContinue
if ($sunshine) {{
    $sunPath = if ($sunshine.PathName) {{ $sunshine.PathName.Trim('"` \t') }} else {{ '' }}
    $underRetired = $false
    foreach ($dir in @({dirs})) {{
        if ($sunPath -like ($dir + '\*')) {{
            $underRetired = $true
            break
        }}
    }}
    if ($underRetired) {{
        try {{ Stop-Service -Name 'SunshineService' -Force -ErrorAction Stop }} catch {{ Write-Output "  stop SunshineService : $_"; $setupFailed = $true }}
        sc.exe delete 'SunshineService' | Out-Null
        if ($LASTEXITCODE -notin 0, 1060, 1072) {{ $setupFailed = $true }}
    }}
}}
foreach ($dir in @({dirs})) {{
    if (Test-Path -LiteralPath $dir) {{
        Get-CimInstance Win32_Process -ErrorAction SilentlyContinue | Where-Object {{ $_.ExecutablePath -like ($dir + '\*') }} | ForEach-Object {{ Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }}
        try {{ Remove-Item -LiteralPath $dir -Recurse -Force -ErrorAction Stop }} catch {{ Write-Output "  remove $dir : $_"; $setupFailed = $true }}
    }}
}}
foreach ($rule in @({rules})) {{ netsh advfirewall firewall delete rule name="$rule" | Out-Null }}
"#,
        tasks = list(tasks),
        services = list(services),
        dirs = list(dirs),
        rules = list(rules)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn retirement_is_scoped_to_the_owned_folders_services_tasks_and_rules() {
        let s = retire_windows_ps();
        for name in [
            "LatchStream",
            "BroLinkStream",
            "LatchEngineUser",
            "BroLinkEngineUser",
            "Latch Streaming TCP",
            "Latch Streaming UDP",
        ] {
            assert!(s.contains(&format!("'{name}'")), "{name}");
        }
        assert!(s.contains("SunshineService"));
        assert!(s.contains("if (Test-Path -LiteralPath $dir)"));
        assert!(s.contains("Remove-Item -LiteralPath $dir -Recurse"));
        assert!(!s.contains("Move-Item"));
    }

    #[test]
    fn the_retire_step_can_be_dumped_for_a_windows_run() {
        let Ok(out) = std::env::var("LATCH_DUMP_RETIRE_SCRIPT") else {
            return;
        };
        std::fs::write(out, retire_windows_ps()).expect("write retire script");
    }
}
