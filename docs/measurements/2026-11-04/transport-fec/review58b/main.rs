// Shim for main (3d5d69e / ee3176a engine): as this.rs, in a main checkout.
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
    pub fn enc_budget(c: &Controller) -> u32 { c.budget() }
    pub fn pace(c: &Controller) -> u32 { if c.pacing() { c.budget() } else { 0 } }
    pub fn video_budget(w: u32, fec: u8, _burst: u8, _fps: u32, _protected: bool) -> u32 { congestion::video_budget(w, fec, false) }
    pub fn burst(_tx: &Packetizer) -> u8 { 0 }
    pub fn set_tx_burst(_tx: &mut Packetizer, _b: u8) {}
    pub fn set_burst(_r: &mut Report, _rx: &mut Reassembler) {}
    #[derive(Default)]
    pub struct Meter;
    pub fn probe(_c: &mut Controller, _now: Instant) -> Option<u8> { None }
    pub fn train(_id: u16, _count: u8) -> Vec<Vec<u8>> { Vec::new() }
    pub fn metered(_m: &mut Meter, _dg: &[u8], _now: Instant) -> bool { false }
    pub fn set_probe(_r: &mut Report, _m: &mut Meter, _now: Instant) {}
    #[derive(Default)]
    pub struct Fec(f32);
    impl Fec { pub fn update(&mut self, r: &Report, c: &Controller, e: Duration) -> (u8, u8) {
        self.0 = latch_proto::video::fec_loss_sample(self.0, r.loss_pct, e);
        (latch_proto::video::fec_percent_for(self.0, c.base_rtt()).min(30), 0)
    } }
}
include!("body.rs");
include!("scenarios.rs");
