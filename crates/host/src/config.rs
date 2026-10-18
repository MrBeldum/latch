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
    /// Sunshine web-UI login the host uses to accept pairing PINs. Written
    /// by the setup step, which sets the same values on Sunshine.
    pub sunshine_user: String,
    pub sunshine_pass: String,
}

impl Default for HostConfig {
    fn default() -> Self {
        Self {
            power_allowed: true,
            start_with_windows: true,
            stay_awake: true,
            sunshine_user: String::new(),
            sunshine_pass: String::new(),
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
    /// file left alone. Defaults mean remote power on and no engine login.
    pub fn try_load() -> anyhow::Result<Self> {
        let path = latch_core::config::data_dir()?.join(FILE);
        if std::fs::metadata(&path).is_ok_and(|m| m.len() == 0) {
            anyhow::bail!("{} is empty", path.display());
        }
        latch_core::config::try_load(FILE)
    }
    /// Overlay this copy's settings on the file rather than replacing it:
    /// `--setup` or a container's bootstrap may have written the engine
    /// login since this copy was loaded, and an empty login here never
    /// erases one on disk.
    pub fn save(&self) -> anyhow::Result<()> {
        latch_core::config::update(FILE, |disk: &mut Self| {
            disk.power_allowed = self.power_allowed;
            disk.start_with_windows = self.start_with_windows;
            disk.stay_awake = self.stay_awake;
            if self.has_creds() {
                disk.sunshine_user = self.sunshine_user.clone();
                disk.sunshine_pass = self.sunshine_pass.clone();
            }
        })
    }

    pub fn has_creds(&self) -> bool {
        !self.sunshine_user.is_empty() && !self.sunshine_pass.is_empty()
    }
}

/// A fresh login for Sunshine's web UI: letters and digits only, so it
/// survives every quoting layer between here and `sunshine --creds`.
pub fn random_password() -> String {
    use rand::Rng;
    const CHARS: &[u8] = b"abcdefghjkmnpqrstuvwxyzABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    let mut rng = rand::thread_rng();
    (0..20)
        .map(|_| CHARS[rng.gen_range(0..CHARS.len())] as char)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_is_long_and_quote_safe() {
        let p = random_password();
        assert_eq!(p.len(), 20);
        assert!(p.chars().all(|c| c.is_ascii_alphanumeric()));
        assert_ne!(p, random_password());
    }

    #[test]
    fn defaults_allow_power_and_autostart_but_have_no_creds() {
        let c = HostConfig::default();
        assert!(c.power_allowed);
        assert!(c.start_with_windows);
        assert!(c.stay_awake);
        assert!(!c.has_creds());
        // An old host.toml without the field gets the default too.
        let c: HostConfig = toml::from_str("power_allowed = false").unwrap();
        assert!(!c.power_allowed && c.start_with_windows && c.stay_awake);
    }
}
