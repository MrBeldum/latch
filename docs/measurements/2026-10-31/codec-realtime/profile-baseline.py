#!/usr/bin/env python3
"""Instrument the temporary baseline crate created by build-baseline.py.
Stage rows: profile, preparation_ms, workers_ms, serial_deblock_ms, passes.
"""
import pathlib
import sys
p = pathlib.Path(sys.argv[1]) / 'src/h264/encode.rs'
s = p.read_text().replace('        self.validate(picture)?;', '        self.validate(picture)?;\n        let profile_start = std::time::Instant::now();')
s = s.replace('        let (data, rec, mbs, skipped, qp_sum) = loop {', '        let profile_prepared = profile_start.elapsed().as_secs_f64() * 1000.;\n        let profile_work = std::time::Instant::now();\n        let mut passes = 0;\n        let (data, rec, mbs, skipped, qp_sum) = loop {\n            passes += 1;')
s = s.replace('        let mut rec = rec;\n        deblock::filter(&mut rec, &mbs);', '        let profile_workers = profile_work.elapsed().as_secs_f64() * 1000.;\n        let mut rec = rec;\n        let profile_filter = std::time::Instant::now();\n        deblock::filter(&mut rec, &mbs);\n        eprintln!("profile,{:.3},{:.3},{:.3},{}", profile_prepared, profile_workers, profile_filter.elapsed().as_secs_f64() * 1000.,passes);')
p.write_text(s)
