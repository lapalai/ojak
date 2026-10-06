#![cfg(target_os = "macos")]
use aam_protocol::{process_alive, process_identity};

#[test]
fn a_reused_pid_cannot_keep_an_old_lease_alive() {
    let mut identity = process_identity(std::process::id()).unwrap();
    assert!(process_alive(&identity));
    identity.started_at = "1:000000".into();
    assert!(!process_alive(&identity));
}

#[test]
fn a_reaped_child_is_distinguished_from_an_inspection_failure() {
    let mut child = std::process::Command::new("/bin/sleep")
        .arg("10")
        .spawn()
        .unwrap();
    let identity = process_identity(child.id()).unwrap();
    child.kill().unwrap();
    child.wait().unwrap();
    assert_eq!(
        process_identity(identity.pid).unwrap_err().code,
        "PROCESS_NOT_FOUND"
    );
    assert!(!process_alive(&identity));
}
