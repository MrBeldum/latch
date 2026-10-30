//! Latch viewer: machine list and stream window.

pub mod app;
pub mod clipboard;
pub mod config;
pub mod display;
pub mod handover;
pub mod input;
pub mod macos;
pub mod path;
mod refresh;
pub mod session;
pub mod settings;
pub mod share;
pub mod stream;
pub mod update;
pub mod video;

/// How a Latch window opens: the latch-host app and the development
/// viewer alike.
pub fn native_options() -> eframe::NativeOptions {
    let icon = if cfg!(target_os = "macos") {
        // eframe calls NSApplication.setApplicationIconImage with this
        // bitmap, which replaces Latch.icns in the Dock with an unmasked
        // square. An empty icon leaves the bundle icon in place.
        egui::IconData::default()
    } else {
        egui::IconData {
            rgba: latch_core::icon::render(64),
            width: 64,
            height: 64,
        }
    };
    eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1100.0, 720.0])
            .with_min_inner_size([640.0, 420.0])
            .with_title("Latch")
            .with_icon(icon),
        renderer: eframe::Renderer::Wgpu,
        // A decoded frame goes to the screen as soon as it is drawn rather
        // than waiting for the display's next refresh: up to a whole
        // refresh interval less between the PC's picture and the eye. The
        // picture only changes when a frame arrives, so there is nothing
        // to tear.
        vsync: false,
        wgpu_options: egui_wgpu::WgpuConfiguration {
            present_mode: egui_wgpu::wgpu::PresentMode::AutoNoVsync,
            desired_maximum_frame_latency: Some(1),
            ..Default::default()
        },
        ..Default::default()
    }
}
