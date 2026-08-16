use std::fs;
use std::path::Path;
use std::process::Command;

use serde_json::Value;
use tempfile::TempDir;

#[test]
fn lux_tcp_ip_stack_passes_protocol_vectors() {
    let temp_dir = TempDir::new().expect("temp dir");
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/tcp_ip.lux");
    let compile = Command::new(env!("CARGO_BIN_EXE_lux"))
        .env("LUX_HOME", temp_dir.path())
        .arg(&source)
        .output()
        .expect("lux compiler should start");
    assert!(
        compile.status.success(),
        "TCP/IP stack compilation failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&compile.stdout),
        String::from_utf8_lossy(&compile.stderr),
    );

    let artifacts = temp_dir.path().join("artifacts");
    let metadata: Value = serde_json::from_slice(
        &fs::read(artifacts.join("tcp_ip.meta.json")).expect("TCP/IP metadata"),
    )
    .expect("valid TCP/IP metadata");
    let module = metadata["entry"]["module"].as_str().expect("entry module");
    let eval = format!("io:format(\"~p~n\", ['{module}':apply()]), halt().");
    let run = Command::new("erl")
        .arg("-noshell")
        .arg("-pa")
        .arg(&artifacts)
        .arg("-eval")
        .arg(eval)
        .output()
        .expect("Erlang should start");

    assert!(
        run.status.success(),
        "TCP/IP stack execution failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&run.stdout),
        String::from_utf8_lossy(&run.stderr),
    );
    assert_eq!(String::from_utf8_lossy(&run.stdout).trim(), "true");
}
