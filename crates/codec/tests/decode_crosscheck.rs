mod decode_compare;
use std::{env, fs, path::PathBuf};
#[test]
fn ffmpeg_is_byte_exact() {
    if env::var_os("H264_CROSSCHECK").is_none() {
        eprintln!("SKIP cross-check: run scripts/h264-crosscheck.sh, then set H264_CROSSCHECK=1");
        return;
    }
    let root = env::var_os("H264_CROSSCHECK_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/conformance/crosscheck")
        });
    let mut paths = fs::read_dir(root)
        .expect("generate cross-check streams")
        .map(|p| p.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "h264"))
        .collect::<Vec<_>>();
    paths.sort();
    assert!(!paths.is_empty());
    let mut failures = Vec::new();
    let mut frames = 0;
    for path in &paths {
        match decode_compare::compare(path, &path.with_extension("yuv")) {
            Ok(n) => {
                eprintln!(
                    "{}: {n} pictures PASS",
                    path.file_name().unwrap().to_string_lossy()
                );
                frames += n;
            }
            Err(e) => failures.push(format!("{}: {e}", path.display())),
        }
    }
    eprintln!(
        "FFmpeg: {} streams, {frames} byte-exact pictures, {} failures",
        paths.len(),
        failures.len()
    );
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
