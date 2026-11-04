#[path = "../src/congestion.rs"]
mod congestion;
use latch_proto::video::{OutFrame, Packetizer};
#[test]
fn protected_budget() {
    for (budget, fps) in [(1000, 240), (5000, 120), (20000, 120), (100000, 120)] {
        for kind in 0..3 {
            let target = congestion::video_budget(budget, 10, 0, fps, false, kind != 0);
            let data = vec![0x5a; (target * 1000 / 8 / fps) as usize];
            let mut p = Packetizer::new(1);
            // Frame 1 can legally reference frame 0.
            if kind == 2 {
                p.packetize(
                    &OutFrame {
                        data: &[],
                        keyframe: true,
                        host_delay: 0,
                    },
                    |_| {},
                );
                p.mark_recovery(0);
            }
            let mut wire = 0;
            p.packetize(
                &OutFrame {
                    data: &data,
                    keyframe: kind == 1,
                    host_delay: 0,
                },
                |dg| wire += dg.len() + 80,
            );
            let wire_kbps = wire * 8 * fps as usize / 1000;
            println!(
                "WIRE budget={budget} fps={fps} target={target} kind={kind} wire_kbps={wire_kbps}"
            );
            let allowance = (budget as usize).max(if kind != 0 {
                5 * 109 * 8 * fps as usize / 1000
            } else {
                0
            });
            assert!(wire_kbps <= allowance, "protected reservation exceeded");
        }
    }
}
