//! A host encoding in software holds a request to what its CPU keeps up
//! with, whatever the viewer asked: its own process, since the ceiling is
//! the engine's for the whole host.
use latch_engine::server::{Options, Server};
use latch_engine::software::{self, Ceiling};
use latch_proto::control::{Codec, StreamRequest};
use std::sync::{Arc, Mutex};

#[test]
fn a_cold_software_host_does_not_take_4k_at_the_display_refresh_rate() {
    std::env::set_var("LATCH_TEST_PATTERN", "still");
    let seen = Arc::new(Mutex::new(Vec::new()));
    let server = Server::bind(
        0,
        Options {
            prepare: Some(Box::new({
                let seen = seen.clone();
                move |req| {
                    seen.lock().unwrap().push((req.width, req.height, req.fps));
                    Ok(None)
                }
            })),
            ..Options::default()
        },
    )
    .unwrap();
    let peer = "127.0.0.1:12345".parse().unwrap();
    let request = StreamRequest {
        protocol: latch_proto::PROTOCOL,
        width: 3840,
        height: 2160,
        fps: 120,
        bitrate_kbps: 160_000,
        codecs: vec![Codec::H264Baseline],
        udp_port: 9,
        audio: false,
        name: "cold".into(),
        cursor: false,
        gamepads: false,
    };
    software::set_ceiling(Some(Ceiling::PROVISIONAL));
    drop(server.prepare(request.clone(), peer).unwrap());
    // Measured: 1080p at 120 is 4K at 30.
    software::set_ceiling(Some(Ceiling {
        width: 1920,
        height: 1080,
        fps: 120,
    }));
    drop(server.prepare(request.clone(), peer).unwrap());
    // A hardware host sets none, and the request is taken as asked.
    software::set_ceiling(None);
    drop(server.prepare(request, peer).unwrap());
    assert_eq!(
        *seen.lock().unwrap(),
        [(1920, 1080, 30), (3840, 2160, 30), (3840, 2160, 120)]
    );
}
