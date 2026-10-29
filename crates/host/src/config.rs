//! The host's few settings, in `%LOCALAPPDATA%\Latch\host.toml`.

use serde::{Deserialize, Serialize};

pub const FILE: &str = "host.toml";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct HostConfig {
    /// Let a Mac on the tailnet sleep, restart, or shut this PC down.
    pub power_allowed: bool,
    /// Keep the background service registered to start at logon. On by
    /// default: a PC nobody can walk up to must come back reachable after
    /// every restart. The service re-adds the registry entry if it is gone.
    pub start_with_windows: bool,
    /// Do not idle-sleep when plugged in. Tailscale dies with the PC; a Mac
    /// on another network cannot wake it. On by default. Explicit Sleep
    /// from the Mac or the Start menu still works.
    pub stay_awake: bool,
    /// Take a newer `latch-host.exe` that a machine on the owner's account
    /// sends (`/v1/update`), which is how a Mac keeps its PCs current. Off,
    /// the PC refuses every push and is updated only at the PC itself.
    pub accept_updates: bool,
    /// Shell commands run before and after a stream, with the viewer's size.
    pub prep_do: String,
    pub prep_undo: String,
}

impl Default for HostConfig {
    fn default() -> Self {
        Self {
            power_allowed: true,
            start_with_windows: true,
            stay_awake: true,
            accept_updates: true,
            prep_do: String::new(),
            prep_undo: String::new(),
        }
    }
}

impl HostConfig {
    pub fn load() -> Self {
        latch_core::config::load(FILE)
    }

    /// Read host.toml for a service that already runs on settings: an error
    /// rather than defaults for a file that cannot be read, does not parse,
    /// or is empty (an editor saving in place truncates it first), and the
    /// file left alone. Defaults mean remote power on and no prep command.
    pub fn try_load() -> anyhow::Result<Self> {
        let path = latch_core::config::data_dir()?.join(FILE);
        if std::fs::metadata(&path).is_ok_and(|m| m.len() == 0) {
            anyhow::bail!("{} is empty", path.display());
        }
        latch_core::config::try_load(FILE)
    }
    /// Update the window's settings, preserving prep commands edited on disk.
    pub fn save(&self) -> anyhow::Result<()> {
        latch_core::config::update(FILE, |disk: &mut Self| {
            disk.power_allowed = self.power_allowed;
            disk.start_with_windows = self.start_with_windows;
            disk.stay_awake = self.stay_awake;
            disk.accept_updates = self.accept_updates;
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_allow_power_and_autostart() {
        let c = HostConfig::default();
        assert!(c.power_allowed);
        assert!(c.start_with_windows);
        assert!(c.stay_awake);
        assert!(c.accept_updates, "pushed updates stay on unless turned off");
        assert!(c.prep_do.is_empty() && c.prep_undo.is_empty());
        let old: HostConfig =
            toml::from_str("sunshine_user = \"old\"\nsunshine_pass = \"secret\"").unwrap();
        assert_eq!(old, c);
        // An old host.toml without the field gets the default too.
        let c: HostConfig = toml::from_str("power_allowed = false").unwrap();
        assert!(!c.power_allowed && c.start_with_windows && c.stay_awake && c.accept_updates);
        let c: HostConfig = toml::from_str("accept_updates = false").unwrap();
        assert!(!c.accept_updates);
    }
}
