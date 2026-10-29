//! Per-session desktop preparation, including the Latch 4 command migration.
use crate::config::HostConfig;
#[cfg(target_os = "linux")]
use crate::config::FILE;
use anyhow::{Context, Result};
use latch_proto::control::StreamRequest;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

type Undo = Option<Box<dyn FnOnce() + Send>>;

pub fn prepare(cfg: &HostConfig, req: &StreamRequest) -> Result<Undo> {
    let size = [
        req.width.clamp(64, 8192) & !1,
        req.height.clamp(64, 8192) & !1,
        req.fps.clamp(1, 240),
    ];
    if !cfg.prep_do.trim().is_empty() {
        let undo = cfg.prep_undo.clone();
        if let Err(e) = run(&cfg.prep_do, size, Duration::from_secs(15)) {
            if !undo.trim().is_empty() {
                let _ = run(&undo, size, Duration::from_secs(15));
            }
            return Err(e);
        }
        return Ok((!undo.trim().is_empty()).then(|| {
            Box::new(move || {
                if let Err(e) = run(&undo, size, Duration::from_secs(15)) {
                    tracing::warn!("could not restore the desktop: {e:#}");
                }
            }) as Box<dyn FnOnce() + Send>
        }));
    }
    if !cfg!(windows) && !crate::streamer::display_follows_viewer() {
        return Ok(None);
    }
    let previous = latch_engine::display::prepare(size[0], size[1], size[2])?;
    Ok(previous.map(|previous| {
        Box::new(move || {
            if let Err(e) = latch_engine::display::restore(&previous) {
                tracing::warn!("could not restore the desktop: {e:#}");
            }
        }) as Box<dyn FnOnce() + Send>
    }))
}

fn run(script: &str, size: [u32; 3], limit: Duration) -> Result<()> {
    #[cfg(windows)]
    let mut cmd = {
        use std::os::windows::process::CommandExt;
        let mut cmd = Command::new("cmd.exe");
        cmd.raw_arg(format!("/D /S /C \"{script}\""))
            .creation_flags(0x0800_0000);
        cmd
    };
    #[cfg(not(windows))]
    let mut cmd = {
        use std::os::unix::process::CommandExt;
        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", script]).process_group(0);
        cmd
    };
    for (name, value) in ["WIDTH", "HEIGHT", "FPS"].into_iter().zip(size) {
        cmd.env(format!("LATCH_CLIENT_{name}"), value.to_string());
        // Existing owner scripts use these names; keep them during migration.
        cmd.env(format!("SUNSHINE_CLIENT_{name}"), value.to_string());
    }
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("start desktop prep command")?;
    let deadline = Instant::now() + limit;
    loop {
        if let Some(status) = child.try_wait()? {
            anyhow::ensure!(
                status.success(),
                "desktop prep command exited with {status}"
            );
            return Ok(());
        }
        if Instant::now() >= deadline {
            #[cfg(not(windows))]
            unsafe {
                unsafe extern "C" {
                    fn kill(pid: i32, signal: i32) -> i32;
                }
                // The shell and its children have their own process group.
                kill(-(child.id() as i32), 9);
            }
            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt;
                let _ = Command::new("taskkill.exe")
                    .args(["/PID", &child.id().to_string(), "/T", "/F"])
                    .creation_flags(0x0800_0000)
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
            }
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!(
                "desktop prep command timed out after {} seconds",
                limit.as_secs()
            );
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// Import only the single global command pair supported by Latch.
pub fn legacy_commands(conf: &str) -> Result<Option<(String, String)>> {
    #[derive(serde::Deserialize)]
    struct Prep {
        #[serde(rename = "do")]
        command: String,
        #[serde(default)]
        undo: String,
    }
    let Some(value) = conf
        .trim_start_matches('\u{feff}')
        .lines()
        .find_map(|line| {
            let (key, value) = line.split_once('=')?;
            (key.trim() == "global_prep_cmd").then_some(value.trim())
        })
    else {
        return Ok(None);
    };
    let commands: Vec<Prep> =
        serde_json::from_str(value).context("read the previous desktop prep command")?;
    anyhow::ensure!(commands.len() <= 1, "the previous setup has multiple desktop prep commands; put them in a script and set prep_do / prep_undo in host.toml");
    Ok(commands
        .into_iter()
        .next()
        .filter(|c| !c.command.trim().is_empty())
        .map(|c| (c.command, c.undo)))
}

pub fn migrate() -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        let Some(home) = std::env::var_os("HOME") else {
            return Ok(());
        };
        let source = std::path::PathBuf::from(home).join(".config/sunshine/sunshine.conf");
        migrate_from(&source)?;
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn migrate_from(source: &std::path::Path) -> Result<()> {
    if !HostConfig::load().prep_do.trim().is_empty() {
        return Ok(());
    }
    let conf = match std::fs::read_to_string(source) {
        Ok(conf) => conf,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e).context("read the previous desktop configuration"),
    };
    if let Some((command, undo)) = legacy_commands(&conf)? {
        latch_core::config::update(FILE, |cfg: &mut HostConfig| {
            if cfg.prep_do.trim().is_empty() {
                cfg.prep_do = command;
                cfg.prep_undo = undo;
            }
        })?;
        tracing::info!("imported the desktop prep command into host.toml");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn imports_the_owners_command_and_undo_without_changing_shell_text() {
        let conf = r#"# global_prep_cmd = ignored
encoder = software
global_prep_cmd = [{"do":"/usr/local/bin/stuzha-display $SUNSHINE_CLIENT_WIDTH $SUNSHINE_CLIENT_HEIGHT", "undo":"/usr/local/bin/stuzha-display idle"}]
"#;
        assert_eq!(
            legacy_commands(conf).unwrap(),
            Some((
                "/usr/local/bin/stuzha-display $SUNSHINE_CLIENT_WIDTH $SUNSHINE_CLIENT_HEIGHT"
                    .into(),
                "/usr/local/bin/stuzha-display idle".into()
            ))
        );
        assert_eq!(legacy_commands("other = 1").unwrap(), None);
        assert_eq!(legacy_commands("global_prep_cmd = []").unwrap(), None);
        assert_eq!(
            legacy_commands("global_prep_cmd = [{\"do\":\"resize\"}]").unwrap(),
            Some(("resize".into(), String::new()))
        );
        assert!(legacy_commands("global_prep_cmd = broken").is_err());
        assert!(legacy_commands("global_prep_cmd = [{\"do\":\"one\"}, {\"do\":\"two\"}]").is_err());
    }

    #[cfg(not(windows))]
    #[test]
    fn commands_get_both_environment_names_and_cannot_hang_the_server() {
        run("test \"$LATCH_CLIENT_WIDTH\" = 1280 && test \"$SUNSHINE_CLIENT_HEIGHT\" = 720 && test \"$LATCH_CLIENT_FPS\" = 60", [1280,720,60], Duration::from_secs(2)).unwrap();
        assert!(run("exit 2", [1280, 720, 60], Duration::from_secs(2)).is_err());
        assert!(run("sleep 30", [1280, 720, 60], Duration::from_millis(100))
            .unwrap_err()
            .to_string()
            .contains("timed out"));
    }
}
