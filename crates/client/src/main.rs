#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! The viewer on its own, for development: `cargo run -p latch-client`.
//! What ships is `latch-host`, which is this window plus sharing.

fn main() -> anyhow::Result<()> {
    #[cfg(target_os = "linux")]
    if latch_client::refresh::run_helper() {
        return Ok(());
    }
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_target(false)
        .init();
    eframe::run_native(
        "Latch",
        latch_client::native_options(),
        Box::new(|cc| Ok(Box::new(latch_client::app::ClientApp::new(cc)))),
    )
    .map_err(|e| anyhow::anyhow!("{e}"))
}
