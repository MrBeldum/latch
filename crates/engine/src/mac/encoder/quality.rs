use super::*;

#[test]
#[ignore]
fn quality_benchmark() {
    let case = std::env::var("LATCH_VT_QUALITY")
        .unwrap_or_else(|_| "hevc:1920:1080:60:20000:sweep".into());
    let parts: Vec<_> = case.split(':').collect();
    let codec = if parts[0] == "hevc" {
        Codec::Hevc
    } else {
        Codec::H264High
    };
    let w: usize = parts[1].parse().unwrap();
    let h: usize = parts[2].parse().unwrap();
    let fps: u32 = parts[3].parse().unwrap();
    let kbps: u32 = parts[4].parse().unwrap();
    let motion = parts[5];
    let mut base = vec![128u8; w * h];
    for y in 0..h {
        for x in 0..w {
            let mut hash = (x as u32).wrapping_mul(0x45d9f3b) ^ (y as u32).wrapping_mul(0x119de1f3);
            hash ^= hash >> 16;
            hash = hash.wrapping_mul(0x45d9f3b);
            hash ^= hash >> 16;
            base[y * w + x] = (48 + ((x / 8 + y / 8) % 128) as i32 + (hash % 81) as i32 - 40)
                .clamp(16, 235) as u8;
        }
    }
    let mut raw = Vec::new();
    let mut pool = Vec::new();
    for f in 0..12 {
        let image = create_surface_sized(w, h);
        if motion == "noise" {
            for (i, b) in base.iter_mut().enumerate() {
                let mut v = (i as u32).wrapping_add(f * 192837).wrapping_mul(0x45d9f3b);
                v ^= v >> 16;
                v = v.wrapping_mul(0x119de1f3);
                *b = 16 + (v % 220) as u8;
            }
        }
        raw.push(fill_surface_pattern(
            &image,
            &base,
            if motion == "still" { 0 } else { f as usize },
            w,
            h,
        ));
        pool.push(image);
    }
    let mut enc = Encoder::create(
        &VideoConfig {
            width: w as u32,
            height: h as u32,
            fps,
            bitrate_kbps: kbps,
            codec,
        },
        true,
    )
    .unwrap();
    let mut outputs = Vec::new();
    let mut pacer = crate::video::Pacer::new(fps);
    let start = Instant::now();
    for f in 0..(fps * 2) as usize {
        pacer.wait();
        while enc.in_flight() >= 2 {
            if let Some((i, t, frame)) = enc.collect(Duration::from_secs(2)) {
                let frame = frame.map(Result::unwrap);
                let ms = frame
                    .as_ref()
                    .map_or(0., |x| (t - x.captured).as_secs_f64() * 1000.);
                outputs.push((i as usize, frame, ms));
            }
        }
        let now = Instant::now();
        pacer.taken(now);
        enc.submit(pool[f % 12].0, now, f % fps as usize == 0)
            .unwrap();
    }
    while enc.in_flight() > 0 {
        if let Some((i, t, frame)) = enc.collect(Duration::from_secs(2)) {
            let frame = frame.map(Result::unwrap);
            let ms = frame
                .as_ref()
                .map_or(0., |x| (t - x.captured).as_secs_f64() * 1000.);
            outputs.push((i as usize, frame, ms));
        }
    }
    while let Some((i, t, frame)) = enc.collect(Duration::ZERO) {
        let frame = frame.map(Result::unwrap);
        let ms = frame
            .as_ref()
            .map_or(0., |x| (t - x.captured).as_secs_f64() * 1000.);
        outputs.push((i as usize, frame, ms));
    }
    let wall = start.elapsed().as_secs_f64();
    let dir = std::path::PathBuf::from(
        std::env::var("LATCH_VT_OUTPUT").expect("LATCH_VT_OUTPUT directory"),
    );
    fs::create_dir_all(&dir).unwrap();
    let mut stream = fs::File::create(dir.join("stream.bin")).unwrap();
    let mut sources = fs::File::create(dir.join("source.nv12")).unwrap();
    let mut sizes = Vec::new();
    let mut keys = Vec::new();
    let mut lat = Vec::new();
    for (f, frame, ms) in outputs {
        if let Some(frame) = frame {
            stream.write_all(&frame.data).unwrap();
            sources.write_all(&raw[f % 12]).unwrap();
            sizes.push(frame.data.len() as f64);
            if frame.keyframe {
                keys.push(frame.data.len() as f64);
            }
            lat.push(ms);
        }
    }
    drop(stream);
    drop(sources);
    let mean = sizes.iter().sum::<f64>() / sizes.len() as f64;
    let sd = (sizes.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / sizes.len() as f64).sqrt();
    lat.sort_by(|a, b| a.partial_cmp(b).unwrap());
    println!("QUALITY {codec:?} {fps} {kbps} {motion} frames={} fps={:.3} mbps={:.3} wall_mbps={:.3} IDR={:.0} max_idr={:.0} mean={mean:.0} sd={sd:.0} max={:.0} p50ms={:.2} p95ms={:.2}",sizes.len(),sizes.len() as f64/wall,mean*fps as f64*8./1e6,sizes.iter().sum::<f64>()*8./wall/1e6,keys.iter().sum::<f64>()/keys.len() as f64,keys.iter().copied().fold(0.,f64::max),sizes.iter().copied().fold(0.,f64::max),lat[lat.len()/2],lat[lat.len()*95/100]);
}
