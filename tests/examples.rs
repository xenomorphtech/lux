use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;
use tempfile::TempDir;

fn example_paths() -> Vec<PathBuf> {
    let mut paths: Vec<_> = fs::read_dir(Path::new(env!("CARGO_MANIFEST_DIR")).join("examples"))
        .expect("examples directory should exist")
        .map(|entry| entry.expect("dir entry").path())
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("lux"))
        .collect();
    paths.sort();
    paths
}

fn metadata_files(artifacts_dir: &Path) -> Vec<PathBuf> {
    if !artifacts_dir.exists() {
        return Vec::new();
    }
    let mut files: Vec<_> = fs::read_dir(artifacts_dir)
        .expect("artifacts dir should be readable")
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("json"))
        .collect();
    files.sort();
    files
}

fn run_lux(example: &Path, lux_home: &Path, artifacts_dir: &Path) -> PathBuf {
    let before = metadata_files(artifacts_dir);
    let status = Command::new(env!("CARGO_BIN_EXE_lux"))
        .env("LUX_HOME", lux_home)
        .arg(example)
        .status()
        .expect("lux binary should start");
    assert!(
        status.success(),
        "compiling example failed: {}",
        example.display()
    );
    let after = metadata_files(artifacts_dir);
    after
        .into_iter()
        .find(|path| !before.contains(path))
        .expect("compile should create one metadata file")
}

fn run_entry(meta_path: &Path, artifacts_dir: &Path) {
    let meta: Value = serde_json::from_slice(&fs::read(meta_path).expect("metadata file")).unwrap();
    let module = meta["entry"]["module"]
        .as_str()
        .expect("entry module hash")
        .to_string();
    let function = meta["entry"]["function"]
        .as_str()
        .expect("entry function")
        .to_string();
    assert_eq!(function, "apply");
    assert!(meta["entry"].get("arity").is_none());

    let eval = format!("'{}':{}(), halt().", module, function);
    let status = Command::new("erl")
        .arg("-noshell")
        .arg("-pa")
        .arg(artifacts_dir)
        .arg("-eval")
        .arg(eval)
        .status()
        .expect("erl should start");
    assert!(
        status.success(),
        "running example entry failed: {}",
        meta_path.display()
    );
}

#[test]
fn compile_and_run_all_examples() {
    let temp_dir = TempDir::new().expect("temp dir");
    let lux_home = temp_dir.path();
    let artifacts_dir = lux_home.join("artifacts");

    for example in example_paths() {
        let meta_path = run_lux(&example, lux_home, &artifacts_dir);
        run_entry(&meta_path, &artifacts_dir);
    }
}
