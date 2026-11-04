#[path="../../../../../crates/proto/src/probe.rs"] mod probe;
use std::time::{Duration,Instant};
fn main(){
for cap in [1000.,5000.,20000.,100000.,500000.] {
for count in [6u8,16] {
for quant in [0.,1.] {
for loss in [false,true] {
let mut m=probe::TrainMeter::default(); let start=Instant::now();
for i in 0..count {if loss && i%2==1 && i!=count-1 {continue} let ms=i as f64*1280.*8./cap; let ms=if quant>0. {(ms/quant).ceil()*quant} else {ms}; m.push(1,i,count,1200,start+Duration::from_secs_f64(ms/1000.));}
println!("cap={cap} n={count} quant_ms={quant} alternate_loss={loss} estimate={}",m.take_kbps(start+Duration::from_secs(1)));
}}}}
for gap in [100u64,500,1000,2000,4000] { let mut m=probe::TrainMeter::default();let start=Instant::now();for i in 0..6 {m.push(1,i,6,1200,start+Duration::from_micros(i as u64*gap));}println!("100Mbps batched/wifi gap_us={gap} estimate={}",m.take_kbps(start+Duration::from_secs(1))); }
}
