#[allow(dead_code)]
#[path = "../src/congestion.rs"]
mod congestion;
use latch_proto::video::{OutFrame, Packetizer};
#[test]
fn protected_budget() {
    for (budget, fps) in [
        (20000, 120),
        (1000, 240),
        (5000, 120),
        (100000, 120),
        (2000, 60),
        (1000, 60),
    ] {
        for keyframe in [false, true] {
            let target = congestion::video_budget(budget, 10, 0, fps, false, keyframe);
            let data = vec![0x5a; (target * 1000 / 8 / fps).max(1) as usize];
            let mut p = Packetizer::new(1);
            let mut wire = 0;
            p.packetize(
                &OutFrame {
                    data: &data,
                    keyframe,
                    host_delay: 0,
                },
                |dg| wire += dg.len() + 80,
            );
            println!(
                "WIRE budget={budget} fps={fps} target={target} protected={keyframe} wire_kbps={}",
                wire * 8 * fps as usize / 1000
            );
            // recovery frame
            if keyframe {
                let mut p = Packetizer::new(1);
                let mut w0 = 0;
                p.packetize(
                    &OutFrame {
                        data: &[1],
                        keyframe: true,
                        host_delay: 0,
                    },
                    |_| {},
                );
                p.mark_recovery(0);
                p.packetize(
                    &OutFrame {
                        data: &data,
                        keyframe: false,
                        host_delay: 0,
                    },
                    |dg| w0 += dg.len() + 80,
                );
                println!(
                    "WIRE budget={budget} fps={fps} recovery wire_kbps={}",
                    w0 * 8 * fps as usize / 1000
                );
            }
        }
    }
}
