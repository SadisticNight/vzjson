use std::fs;
use std::path::Path;
use vzjson::{Doc, ReadFlags};

#[test]
fn json_test_suite_parsing() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("JSONTestSuite")
        .join("test_parsing");
    if !dir.is_dir() {
        eprintln!(
            "Skipping json_test_suite_parsing: {} not found. Clone https://github.com/nst/JSONTestSuite into tests/JSONTestSuite to run it.",
            dir.display()
        );
        return;
    }

    let mut entries: Vec<_> = fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("json"))
        .collect();
    entries.sort();

    let mut y_fail = Vec::new();
    let mut n_pass = Vec::new();
    let mut y_count = 0usize;
    let mut n_count = 0usize;
    let mut i_count = 0usize;
    let mut i_accepted = 0usize;

    for path in entries {
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let data = fs::read(&path).unwrap();
        let res = Doc::read(&data, ReadFlags::NOFLAG);
        if name.starts_with("y_") {
            y_count += 1;
            if let Err(e) = res {
                y_fail.push(format!("{name}: {e:?}"));
            }
        } else if name.starts_with("n_") {
            n_count += 1;
            if res.is_ok() {
                n_pass.push(name);
            }
        } else if name.starts_with("i_") {
            i_count += 1;
            if res.is_ok() {
                i_accepted += 1;
            }
        }
    }

    eprintln!(
        "JSONTestSuite summary: y={y_count} (failed={}), n={n_count} (accepted={}), i={i_count} (accepted={i_accepted})",
        y_fail.len(),
        n_pass.len()
    );
    assert!(
        y_fail.is_empty(),
        "y_* tests that failed:\n{}",
        y_fail.join("\n")
    );
    assert!(
        n_pass.is_empty(),
        "n_* tests that unexpectedly passed:\n{}",
        n_pass.join("\n")
    );
}
