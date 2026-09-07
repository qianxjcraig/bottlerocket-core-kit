use serde_json::Value;
use std::path::Path;
use std::process::{Command, Output};
use tempfile::TempDir;

fn run(state_path: &Path, arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_accelerator-node-lifecycle"))
        .arg("--state-path")
        .arg(state_path)
        .args(arguments)
        .output()
        .unwrap()
}

fn run_ok(state_path: &Path, arguments: &[&str]) -> Value {
    let output = run(state_path, arguments);
    assert!(
        output.status.success(),
        "command failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn record(state_path: &Path, generation: &str, token: &str, event: &str) {
    run_ok(
        state_path,
        &[
            "record",
            "--generation",
            generation,
            "--token",
            token,
            "--event",
            event,
        ],
    );
}

#[test]
fn packaged_cli_restores_the_previous_profile() {
    let directory = TempDir::new().unwrap();
    let state_path = directory.path().join("state.json");

    run_ok(
        &state_path,
        &[
            "begin",
            "--token",
            "initial",
            "--profile",
            "shared-inference",
            "--expected-generation",
            "0",
        ],
    );
    for event in [
        "node-drained",
        "advertisement-withdrawn",
        "target-profile-applied",
        "target-dra-validated",
        "target-qualified",
        "target-committed",
    ] {
        record(&state_path, "1", "initial", event);
    }

    run_ok(
        &state_path,
        &[
            "begin",
            "--token",
            "training",
            "--profile",
            "distributed-training",
            "--expected-generation",
            "1",
        ],
    );
    record(&state_path, "2", "training", "node-drained");
    run_ok(
        &state_path,
        &[
            "restore",
            "--generation",
            "2",
            "--token",
            "training",
            "--reason",
            "qualification failed",
        ],
    );
    for event in [
        "previous-profile-applied",
        "previous-dra-validated",
        "previous-profile-committed",
    ] {
        record(&state_path, "2", "training", event);
    }

    let status = run_ok(&state_path, &["status"]);
    assert_eq!(
        status["state"]["committed_profile"],
        Value::String("shared-inference".to_string())
    );
    assert_eq!(
        status["state"]["last_result"]["outcome"],
        Value::String("previous-profile-restored".to_string())
    );
    assert!(status["next-action"].is_null());
}

#[test]
fn packaged_cli_rejects_a_stale_transition_id() {
    let directory = TempDir::new().unwrap();
    let state_path = directory.path().join("state.json");
    run_ok(
        &state_path,
        &[
            "begin",
            "--token",
            "current",
            "--profile",
            "shared-inference",
            "--expected-generation",
            "0",
        ],
    );

    let output = run(
        &state_path,
        &[
            "record",
            "--generation",
            "1",
            "--token",
            "stale",
            "--event",
            "node-drained",
        ],
    );

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("transition ID mismatch"));
    let status = run_ok(&state_path, &["status"]);
    assert_eq!(
        status["next-action"],
        Value::String("coordinator-drain-node".to_string())
    );
}
