//! The one Latch window: the machine list to connect from, this
//! machine's Sharing page, and settings. The viewer (`latch-client`)
//! draws the window; the Sharing page is drawn by [`HostApp`] through
//! [`SharePage`], and a summary of it is handed over for the machine list.

use crate::app::{HostApp, Os, Shared};
use eframe::egui;
use latch_client::app::ClientApp;
use latch_client::share::{LocalShare, SharePage, Slot};
use parking_lot::Mutex;
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

pub struct NodeApp {
    client: ClientApp,
    host: Rc<RefCell<HostApp>>,
    local: Slot,
    /// The host state last copied into `local`; copied again only when it
    /// moves, so a stream does not pay for it every frame.
    synced: Option<u64>,
}

/// The Sharing page, as the viewer's window hosts it.
struct HostPage(Rc<RefCell<HostApp>>);

impl SharePage for HostPage {
    fn show(&mut self, ui: &mut egui::Ui) {
        self.0.borrow_mut().page(ui);
    }
}

impl NodeApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let client = ClientApp::new(cc);
        let host = HostApp::new(cc);
        Self::assemble(client, host)
    }

    fn assemble(client: ClientApp, host: HostApp) -> Self {
        let host = Rc::new(RefCell::new(host));
        let local: Slot = Arc::new(Mutex::new(LocalShare::default()));
        let client = client.with_local(local.clone(), Box::new(HostPage(host.clone())));
        Self {
            client,
            host,
            local,
            synced: None,
        }
    }

    /// The whole window with no threads, no settings read and no actions
    /// that reach the system, for rendering in tests.
    #[doc(hidden)]
    pub fn headless(
        cc: &eframe::CreationContext<'_>,
        discovery: latch_client::session::Discovery,
        shared: Shared,
        os: Os,
    ) -> Self {
        let client = ClientApp::headless(
            cc,
            discovery,
            Default::default(),
            latch_client::config::ClientConfig::default(),
        );
        let host = HostApp::headless(Arc::new(Mutex::new(shared)), os);
        Self::assemble(client, host)
    }

    pub fn client_mut(&mut self) -> &mut ClientApp {
        &mut self.client
    }

    /// Copy what the machine list shows about this machine, when it has
    /// changed since the last copy.
    fn sync(&mut self) {
        let host = self.host.borrow();
        let shared = host.shared();
        if self.synced == Some(shared.rev) {
            return;
        }
        self.synced = Some(shared.rev);
        let mut g = self.local.lock();
        g.status = shared.status.clone();
        g.setup_running = shared.setup_running;
        g.setup_result = shared.setup_result.clone();
        g.service_stopped = shared.stopped_by_user();
        g.viewer_only = shared.viewer_only;
    }
}

impl eframe::App for NodeApp {
    fn update(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        self.sync();
        self.client.update(ctx, frame);
    }
}
