// Shim for e2fad22 (the first review-fix round): as this.rs, in that checkout.
#[allow(dead_code)]
#[path = "../src/congestion.rs"]
mod congestion;
#[allow(dead_code)]
#[path = "../src/pacing.rs"]
mod pacing;
mod shim {
    use super::congestion::{self, Controller};
    use latch_proto::{control::Report, video::{Packetizer, Reassembler}};
    use std::time::{Duration, Instant};
    pub fn enc_budget(c: &Controller) -> u32 { c.encoder_wire_budget() }
    pub fn pace(c: &Controller) -> u32 { c.pace_kbps() }
    pub fn video_budget(w: u32, fec: u8, _burst: u8, _fps: u32, _protected: bool) -> u32 { congestion::video_budget(w, fec, _burst, _fps, false, _protected) }
    pub fn burst(tx: &Packetizer) -> u8 { tx.fec_burst() }
    pub fn set_tx_burst(tx: &mut Packetizer, b: u8) { tx.set_fec_burst(b) }
    pub fn set_burst(r: &mut Report, rx: &mut Reassembler) { r.loss_burst_packets = rx.take_loss_burst(); }
    #[derive(Default)]
    pub struct Meter;
    pub fn probe(_c: &mut Controller, _now: Instant) -> Option<u8> { None }
    pub fn train(_id: u16, _count: u8) -> Vec<Vec<u8>> { Vec::new() }
    pub fn metered(_m: &mut Meter, _dg: &[u8], _now: Instant) -> bool { false }
    pub fn set_probe(_r: &mut Report, _m: &mut Meter, _now: Instant) {}
    #[derive(Default)]
    pub struct Fec(congestion::FecPolicy);
    impl Fec { pub fn update(&mut self, r: &Report, c: &Controller, e: Duration) -> (u8, u8) { self.0.update(r, c, e) } }
}
include!("body.rs");
include!("scenarios.rs");
