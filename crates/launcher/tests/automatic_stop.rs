use aam_launcher::supervisor::{supervise_watched, StopTiming};
use std::{process::Command, time::Duration};

#[test]
fn watched_stop_finishes_the_owned_tree_but_normal_exit_is_not_a_handoff() {
    #[cfg(unix)]
    let mut command = {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "sleep 60 & wait"]);
        command
    };
    #[cfg(windows)]
    let mut command = {
        let mut command = Command::new("cmd");
        command.args(["/d", "/c", "start /b ping -n 60 127.0.0.1 >nul & ping -n 60 127.0.0.1 >nul"]);
        command
    };
    command.env_remove("AAM_PARENT_SESSION_ID");
    let mut identity = None;
    let outcome = supervise_watched(command, |root| identity = root.cloned(), |_| true, StopTiming {
        flush: Duration::from_millis(20),
        grace: Duration::from_millis(300),
        kill_wait: Duration::from_secs(3),
    }).unwrap_or_else(|failure| panic!("{}", failure.error.code));
    assert!(outcome.stopped);
    assert!(!outcome.interrupted);
    assert!(!outcome.status.success());
    assert_eq!(outcome.background_processes, Some(Vec::new()));
    assert!(!aam_protocol::process_alive(&identity.unwrap()));

    #[cfg(unix)]
    let command = Command::new("/usr/bin/true");
    #[cfg(windows)]
    let command = {
        let mut command = Command::new("cmd");
        command.args(["/d", "/c", "exit 0"]);
        command
    };
    let outcome = supervise_watched(command, |_| {}, |_| false, StopTiming::default())
        .unwrap_or_else(|failure| panic!("{}", failure.error.code));
    assert!(outcome.status.success());
    assert!(!outcome.stopped);
    assert!(!outcome.interrupted);
    assert_eq!(outcome.background_processes, Some(Vec::new()));
}
