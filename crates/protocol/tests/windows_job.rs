#![cfg(windows)]
//! 서비스는 실행기가 만든 이름 있는 Job을 lease.started 때 열어 두고, 활성 프로세스 수 0만 전체 종료로 인정한다.
//! 이름은 마지막 핸들이 닫히면 사라지므로 "열 수 없음"은 근거가 아니다. 두 성질을 실제 프로세스로 확인한다.
use aam_protocol::{job_name, process_identity, spawn_in_job, ProcessJob};
use std::{
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

fn wait_until(mut done: impl FnMut() -> bool, limit: Duration) -> bool {
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        if done() {
            return true;
        }
        thread::sleep(Duration::from_millis(50));
    }
    done()
}

/// cmd가 ping을 백그라운드로 띄우고 바로 끝난다. ping(손자)은 약 3초 더 산다.
fn root_with_lingering_grandchild() -> Command {
    let mut command = Command::new("cmd");
    command
        .args(["/d", "/c", "start", "/b", "ping", "-n", "4", "127.0.0.1"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
}

#[test]
fn held_job_counts_a_grandchild_after_the_launcher_and_root_are_gone() {
    let (mut child, launcher_job) =
        spawn_in_job(&mut root_with_lingering_grandchild(), false, true, true).unwrap();
    let native = process_identity(child.id()).unwrap();
    // 서비스가 lease.started에서 하는 일: 실행기가 살아 있는 동안 이름으로 연다.
    let service_job = ProcessJob::open(&job_name(native.pid, &native.started_at)).unwrap();
    child.wait().unwrap();
    // 실행기가 끝난 상황: 실행기 쪽 핸들을 닫는다(kill_on_close=false라 프로세스는 산다).
    drop(launcher_job);
    drop(child);
    assert!(
        service_job.active_processes().unwrap() > 0,
        "루트가 끝나도 손자가 살아 있으면 활성 프로세스가 남아야 한다"
    );
    assert!(
        wait_until(|| service_job.active_processes().unwrap() == 0, Duration::from_secs(15)),
        "모든 프로세스가 끝나면 활성 프로세스는 0이어야 한다"
    );
}

#[test]
fn a_job_is_not_openable_by_name_once_all_handles_close_even_with_live_processes() {
    let (mut child, launcher_job) =
        spawn_in_job(&mut root_with_lingering_grandchild(), false, true, true).unwrap();
    let native = process_identity(child.id()).unwrap();
    let name = job_name(native.pid, &native.started_at);
    child.wait().unwrap();
    drop(launcher_job);
    // 손자는 아직 살아 있지만 이름은 사라졌다. 그래서 서비스는 이름 조회를 종료 근거로 쓰지 않는다.
    assert!(ProcessJob::open(&name).is_err());
}
