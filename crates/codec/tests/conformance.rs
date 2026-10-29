mod decode_compare;
use std::{
    fs,
    path::{Path, PathBuf},
};

fn files(root: &Path, found: &mut Vec<PathBuf>) {
    for item in fs::read_dir(root).expect("read conformance directory") {
        let path = item.expect("read conformance entry").path();
        if path.is_dir() {
            files(&path, found);
        } else {
            found.push(path);
        }
    }
}

#[test]
fn itu_h264_1_is_byte_exact() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/conformance");
    if !root.is_dir() {
        eprintln!("SKIP ITU H.264.1: run scripts/fetch-h264-conformance.sh");
        return;
    }
    let mut paths = Vec::new();
    files(&root, &mut paths);
    paths.sort();
    let mut failures = Vec::new();
    let mut count = 0;
    let mut frames = 0;
    eprintln!("{:<28} {:>8} result", "ITU stream", "frames");
    for path in paths.iter().filter(|p| {
        p.extension()
            .is_some_and(|e| matches!(e.to_str(), Some("264" | "h264" | "jsv")))
    }) {
        let dir = path.parent().unwrap();
        let cropped = path.with_file_name(format!(
            "{}_nogray.yuv",
            path.file_stem().unwrap().to_string_lossy()
        ));
        let direct = if cropped.is_file() {
            cropped
        } else {
            path.with_extension("yuv")
        };
        let reference = if direct.is_file() {
            &direct
        } else {
            paths
                .iter()
                .find(|p| {
                    p.parent() == Some(dir)
                        && p.extension()
                            .is_some_and(|e| matches!(e.to_str(), Some("yuv" | "qcif")))
                })
                .expect("missing ITU reference YUV")
        };
        let name = path.file_stem().unwrap().to_string_lossy();
        count += 1;
        match decode_compare::compare(path, reference) {
            Ok(n) => {
                frames += n;
                eprintln!("{name:<28} {n:>8} PASS");
            }
            Err(e) => {
                eprintln!("{name:<28} {:>8} FAIL {e}", "-");
                failures.push(format!("{name}: {e}"));
            }
        }
    }
    eprintln!(
        "ITU total: {count} streams, {frames} byte-exact pictures, {} failures",
        failures.len()
    );
    assert!(count > 0, "conformance directory is present but empty");
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
