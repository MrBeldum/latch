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
    use latch_proto::{control::Report, video::{Packetizer, Reassembler}, wire::Datagram};
    use std::time::{Duration, Instant};
    pub const TRAIN_ECHO: bool = true;
    pub fn echo(c: &mut Controller, rtt: f32, now: Instant) { c.observe_train_rtt(rtt, now); }
    pub fn enc_budget(c: &Controller) -> u32 { c.budget() }
    pub fn pace(c: &Controller) -> u32 { c.pace_kbps() }
    pub fn video_budget(w: u32, fec: u8, burst: u8, fps: u32, protected: bool) -> u32 { congestion::video_budget(w, fec, burst, fps, false, protected) }
    pub fn burst(tx: &Packetizer) -> u8 { tx.fec_burst() }
    pub fn set_tx_burst(tx: &mut Packetizer, b: u8) { tx.set_fec_burst(b) }
    pub fn set_burst(r: &mut Report, rx: &mut Reassembler) { r.loss_burst_packets = rx.take_loss_burst(); }
    pub type Meter = latch_proto::probe::TrainMeter;
    pub fn probe(c: &mut Controller, now: Instant) -> Option<u8> { c.probe_train(now) }
    pub fn train(id: u16, count: u8) -> Vec<Vec<u8>> {
        (0..count).map(|i| { let mut b = Vec::new(); latch_proto::wire::write_probe(&mut b, 1, id, i, count, latch_proto::MAX_DATAGRAM); b }).collect()
    }
    pub fn metered(m: &mut Meter, dg: &[u8], now: Instant) -> bool {
        match latch_proto::wire::parse(dg) {
            Ok(Datagram::Probe { train, index, count, len, .. }) => { m.push(train, index, count, len, now); true }
            _ => false,
        }
    }
    pub fn set_probe(r: &mut Report, m: &mut Meter, now: Instant) { r.probe_kbps = m.take_kbps(now); }
    #[derive(Default)]
    pub struct Fec(congestion::FecPolicy);
    impl Fec { pub fn update(&mut self, r: &Report, c: &Controller, e: Duration) -> (u8, u8) { self.0.update(r, c, e) } }
}
include!("body.rs");
include!("../review58b/scenarios.rs");
