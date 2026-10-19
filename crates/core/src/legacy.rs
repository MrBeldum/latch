//! Compatibility identifiers used by versions before 4.1.0.
//!
//! Installed copies, other machines on the tailnet and old settings
//! folders keep the old names for a while after an upgrade, so the code
//! that recognises or retires them is all here and nowhere else. Nothing
//! in this file is shown to a person except as part of a migration.

use std::ffi::OsString;
use std::path::PathBuf;

/// `Status::app` from a 4.0 host or older.
pub const APP_ID: &str = "brolink";
/// `Streamer::kind` of an engine that an old Latch set up.
pub const STREAMER_KIND: &str = "BroLink";
/// The update push's headers, which a 4.0 host requires and a 4.0 Mac sends.
pub const UPDATE_VERSION_HEADER: &str = "x-brolink-version";
pub const UPDATE_SHA256_HEADER: &str = "x-brolink-sha256";

/// Whether `app` is what a Latch node (this version or an older one) puts in
/// `Status::app`.
pub fn is_node_app(app: &str) -> bool {
    app == crate::APP_ID || app == APP_ID
}

/// Whether `kind` names a streaming engine that Latch set up, under either name.
pub fn is_own_streamer_kind(kind: &str) -> bool {
    kind == crate::STREAMER_KIND || kind == STREAMER_KIND
}

/// A setting from the environment under its current name, else under the
/// old one from before 4.1.0. Unset and empty are the same.
pub fn env(name: &str) -> Option<OsString> {
    let current = std::env::var_os(name).filter(|v| !v.is_empty());
    current.or_else(|| {
        let rest = name.strip_prefix("LATCH_")?;
        std::env::var_os(format!("BROLINK_{rest}")).filter(|v| !v.is_empty())
    })
}

/// Where 4.0 and older kept their settings, pairing identity and logs.
/// `None` when this platform has no per-user data directory.
pub fn data_dir() -> Option<PathBuf> {
    if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA").map(|p| PathBuf::from(p).join("BroLink"))
    } else {
        directories::ProjectDirs::from("dev", "brolink", "BroLink")
            .map(|d| d.data_dir().to_path_buf())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_names_identify_a_node() {
        assert!(is_node_app("latch"));
        assert!(is_node_app("brolink"));
        assert!(!is_node_app("sunshine"));
        assert!(!is_node_app(""));
    }

    #[test]
    fn both_names_identify_our_own_engine() {
        assert!(is_own_streamer_kind("Latch"));
        assert!(is_own_streamer_kind("BroLink"));
        assert!(!is_own_streamer_kind("Sunshine"));
        assert!(!is_own_streamer_kind("Apollo"));
    }

    #[test]
    fn the_old_environment_name_is_a_fallback_and_the_new_one_wins() {
        std::env::remove_var("LATCH_LEGACY_TEST_A");
        std::env::remove_var("BROLINK_LEGACY_TEST_A");
        assert_eq!(env("LATCH_LEGACY_TEST_A"), None);
        std::env::set_var("BROLINK_LEGACY_TEST_A", "old");
        assert_eq!(env("LATCH_LEGACY_TEST_A"), Some("old".into()));
        std::env::set_var("LATCH_LEGACY_TEST_A", "new");
        assert_eq!(env("LATCH_LEGACY_TEST_A"), Some("new".into()));
        std::env::set_var("LATCH_LEGACY_TEST_A", "");
        assert_eq!(env("LATCH_LEGACY_TEST_A"), Some("old".into()));
        std::env::remove_var("LATCH_LEGACY_TEST_A");
        std::env::remove_var("BROLINK_LEGACY_TEST_A");
    }

    #[test]
    fn the_old_data_directory_is_where_4_0_kept_it() {
        let dir = data_dir().expect("a per-user data directory");
        let name = dir.file_name().unwrap().to_string_lossy().to_lowercase();
        assert!(name.contains("brolink"), "{}", dir.display());
    }
}
