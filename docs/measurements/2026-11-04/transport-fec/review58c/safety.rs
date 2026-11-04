#[path="../../../../../crates/proto/src/probe.rs"] mod probe;
#[path="../../../../../crates/proto/src/sim.rs"] mod sim;
use std::time::{Duration,Instant};
fn main(){ for cap in [1000,5000,20000,100000,500000] {for count in [6u8,16] {let start=Instant::now();let mut link=sim::Link::new(sim::Impairment::NONE,17);link.set_bandwidth(cap,Duration::from_millis(4));for i in 0..count {let mut b=vec![0;1200];b[0]=i;link.send(start,&b)}let peak=link.queue_delay(start);let mut m=probe::TrainMeter::default();let mut got=0;for us in (0..200000).step_by(10) {let now=start+Duration::from_micros(us);for b in link.receive(now){got+=1;m.push(1,b[0],count,1200,now)}}println!("cap={cap} count={count} received={got} peak_us={} estimate={}",peak.as_micros(),m.take_kbps(start+Duration::from_secs(1)));}}
}
