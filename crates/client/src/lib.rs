//! Latch viewer: machine list and stream window.

pub mod app;
pub mod clipboard;
pub mod config;
pub mod display;
pub mod handover;
pub mod input;
pub mod macos;
pub mod path;
#[doc(hidden)]
pub mod refresh;
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
    let mut wgpu_options = egui_wgpu::WgpuConfiguration {
        present_mode: egui_wgpu::wgpu::PresentMode::AutoNoVsync,
        desired_maximum_frame_latency: Some(1),
        ..Default::default()
    };
    if let egui_wgpu::WgpuSetup::CreateNew(setup) = &mut wgpu_options.wgpu_setup {
        let descriptor = setup.device_descriptor.clone();
        setup.device_descriptor = std::sync::Arc::new(move |adapter| {
            let mut desc = descriptor(adapter);
            desc.required_features |=
                adapter.features() & egui_wgpu::wgpu::Features::TEXTURE_FORMAT_16BIT_NORM;
            desc
        });
    }
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
        // compositor may still wait for scanout; immediate presentation
        // can tear on displays without a compositor or VRR.
        vsync: false,
        wgpu_options,
        ..Default::default()
    }
}
