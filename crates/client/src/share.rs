//! This machine's sharing side, as the unified window sees it. The host
//! crate fills [`LocalShare`] from the local control service and draws the
//! Sharing page through [`SharePage`]; the viewer only shows a summary row
//! in the machine list and hosts the page in its window.

use latch_core::api::Status;
use parking_lot::Mutex;
use std::sync::Arc;

#[derive(Clone, Default)]
pub struct LocalShare {
    pub status: Option<Status>,
    pub setup_running: bool,
    pub setup_result: Option<Result<(), String>>,
    /// Someone stopped the background service from the Sharing page.
    pub service_stopped: bool,
    /// The owner turned Share this Mac off: this Mac only connects to the
    /// others, and its service is off on purpose.
    pub viewer_only: bool,
}

pub type Slot = Arc<Mutex<LocalShare>>;

/// The Sharing page: setup, what this machine reports,
/// options and diagnostics. Drawn by whoever shares this machine.
pub trait SharePage {
    fn show(&mut self, ui: &mut egui::Ui);
}
