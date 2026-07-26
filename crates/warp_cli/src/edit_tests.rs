use std::{fs, thread, time::Duration};

use super::*;

#[test]
fn hook_payload_serializes_with_the_field_names_the_client_expects() {
    let payload = EditRequest {
        path: "/tmp/kubectl-edit-1234.yaml".to_owned(),
        host: String::new(),
        ack_path: "/tmp/warp-edit-abc.ack".to_owned(),
        done_path: "/tmp/warp-edit-abc.done".to_owned(),
        wait: true,
    };

    let json = serde_json::to_value(&payload).expect("payload serializes");

    assert_eq!(json["path"], "/tmp/kubectl-edit-1234.yaml");
    assert_eq!(json["host"], "");
    assert_eq!(json["ack_path"], "/tmp/warp-edit-abc.ack");
    assert_eq!(json["done_path"], "/tmp/warp-edit-abc.done");
    assert_eq!(json["wait"], true);
}

#[test]
fn the_escape_sequence_is_a_hex_encoded_warp_shell_hook() {
    let request = EditRequest {
        path: "/tmp/kubectl-edit-1234.yaml".to_owned(),
        host: String::new(),
        ack_path: "/tmp/warp-edit-abc.ack".to_owned(),
        done_path: "/tmp/warp-edit-abc.done".to_owned(),
        wait: true,
    };

    let sequence = request.escape_sequence().expect("sequence builds");

    let body = sequence
        .strip_prefix("\x1b]9278;d;")
        .and_then(|body| body.strip_suffix("\x07"))
        .expect("sequence is a bell-terminated OSC 9278 hook");

    let decoded = String::from_utf8(hex::decode(body).expect("payload is hex")).expect("utf-8");
    let parsed: serde_json::Value = serde_json::from_str(&decoded).expect("payload is json");

    assert_eq!(parsed["hook"], "EditFile");
    assert_eq!(parsed["value"]["path"], "/tmp/kubectl-edit-1234.yaml");
}

#[test]
fn paths_that_would_corrupt_the_sequence_survive_encoding() {
    // Hex encoding exists precisely so that a path containing the OSC
    // separator, a terminator, or a newline cannot break out of the payload.
    let hostile_path = "/tmp/we;ird\x07path\nwith\x1bescapes.yaml";
    let request = EditRequest {
        path: hostile_path.to_owned(),
        host: String::new(),
        ack_path: "/tmp/warp-edit-abc.ack".to_owned(),
        done_path: "/tmp/warp-edit-abc.done".to_owned(),
        wait: true,
    };

    let sequence = request.escape_sequence().expect("sequence builds");

    let body = sequence
        .strip_prefix("\x1b]9278;d;")
        .and_then(|body| body.strip_suffix("\x07"))
        .expect("sequence is a bell-terminated OSC 9278 hook");

    assert!(
        body.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "the payload must not be able to terminate the sequence early"
    );
    let decoded = String::from_utf8(hex::decode(body).expect("payload is hex")).expect("utf-8");
    let parsed: serde_json::Value = serde_json::from_str(&decoded).expect("payload is json");

    assert_eq!(parsed["value"]["path"], hostile_path);
}

#[test]
fn marker_paths_are_unique_per_invocation() {
    let first = MarkerPaths::new();
    let second = MarkerPaths::new();

    assert_ne!(first.ack, second.ack);
    assert_ne!(first.done, second.done);
    assert_ne!(first.ack, first.done);
}

#[test]
fn wait_for_marker_gives_up_once_the_timeout_elapses() {
    let dir = tempfile::tempdir().expect("temp dir");
    let missing = dir.path().join("never-written.ack");

    assert!(!wait_for_marker(&missing, Some(Duration::from_millis(150))));
}

#[test]
fn wait_for_marker_returns_once_the_marker_appears() {
    let dir = tempfile::tempdir().expect("temp dir");
    let marker = dir.path().join("eventually.done");

    let writer_marker = marker.clone();
    let writer = thread::spawn(move || {
        thread::sleep(Duration::from_millis(100));
        fs::write(&writer_marker, "0").expect("write marker");
    });

    assert!(wait_for_marker(&marker, Some(Duration::from_secs(5))));
    writer.join().expect("writer thread");
}

#[test]
fn exit_code_is_read_from_the_done_marker() {
    let dir = tempfile::tempdir().expect("temp dir");
    let marker = dir.path().join("edit.done");

    fs::write(&marker, "0\n").expect("write marker");
    assert_eq!(read_exit_code(&marker), 0);

    fs::write(&marker, "1").expect("write marker");
    assert_eq!(read_exit_code(&marker), 1);
}

#[test]
fn a_malformed_done_marker_is_treated_as_success() {
    let dir = tempfile::tempdir().expect("temp dir");
    let marker = dir.path().join("edit.done");

    // A truncated or empty write must not turn a completed edit into a failure,
    // which for `kubectl edit` would mean silently discarding the user's edits.
    fs::write(&marker, "").expect("write marker");
    assert_eq!(read_exit_code(&marker), 0);

    fs::write(&marker, "not-a-number").expect("write marker");
    assert_eq!(read_exit_code(&marker), 0);

    assert_eq!(read_exit_code(&dir.path().join("absent.done")), 0);
}

#[test]
fn relative_paths_are_resolved_against_the_working_directory() {
    let resolved = absolute_path(Path::new("resource.yaml")).expect("resolves");

    assert!(resolved.is_absolute());
    assert!(resolved.ends_with("resource.yaml"));
}

#[test]
fn absolute_paths_are_left_alone() {
    let path = if cfg!(windows) {
        PathBuf::from(r"C:\tmp\resource.yaml")
    } else {
        PathBuf::from("/tmp/resource.yaml")
    };

    assert_eq!(absolute_path(&path).expect("resolves"), path);
}
