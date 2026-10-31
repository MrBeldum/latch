//! A bare Latch host for measurements: the engine behind the same HTTP
//! upgrade the control service uses, without the service, its window or
//! its authorization. Bind it to an address only you can reach.
//!
//!     serve [--bind 100.x.y.z] [--port 47860] [--telemetry]
//!
//! `LATCH_TEST_PATTERN=still|sweep|noise` streams the synthetic desktop.

use latch_core::http::{self, Response};
use latch_engine::server::{Options, Server};
use latch_proto::control::{self, StreamRequest};
use std::net::TcpListener;

fn main() -> anyhow::Result<()> {
    #[cfg(target_os = "linux")]
    if latch_engine::linux::run_helper() {
        return Ok(());
    }
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();
    let args: Vec<String> = std::env::args().collect();
    let arg = |name: &str, default: &str| {
        args.windows(2)
            .find(|w| w[0] == name)
            .map_or(default.to_string(), |w| w[1].clone())
    };
    let bind = arg("--bind", "127.0.0.1");
    let port: u16 = arg("--port", "47860").parse()?;
    let listener = TcpListener::bind((bind.as_str(), port))?;
    let server = Server::bind(
        port,
        Options {
            prepare: None,
            log: Box::new(|line| eprintln!("{line}")),
        },
    )?;
    eprintln!("serving on {bind}:{port} (TCP and UDP)");
    if args.iter().any(|a| a == "--telemetry") {
        let server = server.clone();
        std::thread::spawn(move || {
            let start = std::time::Instant::now();
            loop {
                std::thread::sleep(std::time::Duration::from_secs(1));
                eprintln!(
                    "telemetry elapsed_ms={} {:?}",
                    start.elapsed().as_millis(),
                    server.status()
                );
            }
        });
    }

    http::serve_with_peer_check(
        listener,
        |_| true,
        move |peer, req| {
            if req.path != control::PATH {
                return Response::json(404, &serde_json::json!({ "ok": false }));
            }
            let request: StreamRequest = match req.json() {
                Ok(r) => r,
                Err(e) => {
                    return Response::json(
                        400,
                        &serde_json::json!({ "ok": false, "error": e.to_string() }),
                    )
                }
            };
            match server.prepare(request, peer) {
                Ok(prepared) => {
                    let server = server.clone();
                    Response::switching(control::UPGRADE, move |tcp| server.run(prepared, tcp))
                }
                Err(r) => Response::json(
                    r.status,
                    &serde_json::json!({ "ok": false, "error": r.message }),
                ),
            }
        },
    );
    Ok(())
}
