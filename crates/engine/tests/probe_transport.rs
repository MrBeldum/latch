// Shim for this branch: copy body.rs, scenarios.rs and this file into
// crates/engine/tests/ and run `cargo test --release -p latch-engine --test
// this -- --nocapture` (R58B=<name> filters, R58B_V=1 prints timelines).
#[allow(dead_code)]
#[path = "../src/congestion.rs"]
mod congestion;
#[allow(dead_code)]
#[path = "../src/pacing.rs"]
mod pacing;
mod shim {
    use super::congestion::{self, Controller};
    use latch_proto::{
        control::Report,
        video::{Packetizer, Reassembler},
        wire::Datagram,
    };
    use std::time::{Duration, Instant};
    pub const TRAIN_ECHO: bool = true;
    pub fn echo(c: &mut Controller, rtt: f32, now: Instant) {
        c.observe_train_rtt(rtt, now);
    }
    pub fn enc_budget(c: &Controller) -> u32 {
        c.budget()
    }
    pub fn pace(c: &Controller) -> u32 {
        c.pace_kbps()
    }
    pub fn video_budget(w: u32, fec: u8, burst: u8, fps: u32, protected: bool) -> u32 {
        congestion::video_budget(w, fec, burst, fps, false, protected)
    }
    pub fn burst(tx: &Packetizer) -> u8 {
        tx.fec_burst()
    }
    pub fn set_tx_burst(tx: &mut Packetizer, b: u8) {
        tx.set_fec_burst(b)
    }
    pub fn set_burst(r: &mut Report, rx: &mut Reassembler) {
        r.loss_burst_packets = rx.take_loss_burst();
    }
    pub type Meter = latch_proto::probe::TrainMeter;
    pub fn probe(c: &mut Controller, now: Instant) -> Option<u8> {
        c.probe_train(now)
    }
    pub fn train(id: u16, count: u8) -> Vec<Vec<u8>> {
        (0..count)
            .map(|i| {
                let mut b = Vec::new();
                latch_proto::wire::write_probe(&mut b, 1, id, i, count, latch_proto::MAX_DATAGRAM);
                b
            })
            .collect()
    }
    pub fn metered(m: &mut Meter, dg: &[u8], now: Instant) -> bool {
        match latch_proto::wire::parse(dg) {
            Ok(Datagram::Probe {
                train,
                index,
                count,
                len,
                ..
            }) => {
                m.push(train, index, count, len, now);
                true
            }
            _ => false,
        }
    }
    pub fn set_probe(r: &mut Report, m: &mut Meter, now: Instant) {
        r.probe_kbps = m.take_kbps(now);
    }
    #[derive(Default)]
    pub struct Fec(congestion::FecPolicy);
    impl Fec {
        pub fn update(&mut self, r: &Report, c: &Controller, e: Duration) -> (u8, u8) {
            self.0.update(r, c, e)
        }
    }
}
include!("../../../docs/measurements/2026-11-04/transport-fec/review58c/body.rs");

#[test]
fn sparse_jitter_keeps_the_ceiling_and_clean_resume_keeps_its_latency() {
    for jitter in [0, 4] {
        let s = Scenario {
            name: "sparse_jitter",
            fps: 60,
            ceiling: 100000,
            caps: &[(0, 100000)],
            fifo_ms: 40,
            rtt: 10,
            interval: 50,
            loss: 0.0,
            burst_enter: 0.0,
            jitter_ms: jitter,
            legacy: false,
            content: Content::Sparse {
                kbps: 500,
                idle_until: 0,
            },
            spike: None,
            idr_at: &[],
            secs: 35,
            seed: 17,
        };
        let o = run(&s);
        assert_eq!(o.final_budget, s.ceiling);
        assert_eq!(o.mean_enc, f64::from(s.ceiling));
        assert_eq!((o.lost, o.skipped), (0, 0));
    }
    for (rtt, latency) in [(10, 8), (50, 28), (150, 78), (250, 128)] {
        let s = Scenario {
            name: "clean_resume",
            fps: 60,
            ceiling: 20000,
            caps: &[(0, 100000)],
            fifo_ms: 40,
            rtt,
            interval: 50,
            loss: 0.0,
            burst_enter: 0.0,
            jitter_ms: 0,
            legacy: false,
            content: Content::IdleThenMotion { at: 5000 },
            spike: None,
            idr_at: &[],
            secs: 12,
            seed: 17,
        };
        let o = run(&s);
        assert_eq!(o.full_rate_at, Some(latency));
        assert_eq!((o.lost, o.skipped), (0, 0));
    }
}
