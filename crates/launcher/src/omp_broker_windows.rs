//! Windows omp broker supervision. The always-running `aam-service` starts `omp auth-broker serve`
//! itself (no window, no Task Scheduler, no `conhost --headless`): those launch patterns are what
//! behaviour-based antivirus engines flag on unsigned binaries.
use super::{authenticated_snapshot, digest, native_omp, require_executable};
use aam_protocol::{ApiError, Paths};
use serde::{Deserialize, Serialize};
use std::{fs, os::windows::process::CommandExt, path::PathBuf, process::Command, time::{Duration, Instant}};
fn failure_message(code: &str, message: &str) -> ApiError { ApiError::new(code, message) }

const OWNER: &str = "ai-account-manager.omp-broker";

/// 이전 버전이 만든 작업 스케줄러 broker 작업의 소유 기록. 이전(migration)에만 쓴다.
#[derive(Deserialize)]
struct LegacyReceipt { owner: String, task_name: String, definition_digest: String }
fn legacy_receipt_path(paths: &Paths) -> PathBuf { paths.home.join("omp-broker-task.json") }
fn legacy_task_name(paths: &Paths) -> String {
    format!("Ojak-OMP-Broker-{}", &digest(paths.home.to_string_lossy().as_bytes())[7..23])
}

/// 이 파일이 있으면 로그인 때 이미 뜨는 `aam-service`가 broker를 직접 띄우고 감시한다.
fn service_marker(paths: &Paths) -> PathBuf { paths.home.join("omp-broker-service.json") }
#[derive(Serialize, Deserialize)]
struct ServiceMarker { owner: String, runtime: PathBuf, reason: String }
fn service_runtime(paths: &Paths) -> Option<PathBuf> {
    let bytes = fs::read(service_marker(paths)).ok()?;
    serde_json::from_slice::<ServiceMarker>(&bytes).ok().filter(|marker| marker.owner == OWNER).map(|marker| marker.runtime)
}

pub(super) fn supervised(paths: &Paths) -> bool { service_runtime(paths).is_some() }

fn scheduler(args: &[&str]) -> Result<std::process::Output, ApiError> {
    let root = std::env::var_os("SystemRoot").ok_or_else(|| failure_message("BROKER_AGENT_FAILED", "Windows 시스템 경로가 없어요."))?;
    Command::new(PathBuf::from(root).join(r"System32\schtasks.exe"))
        .args(args).creation_flags(0x0800_0000).output()
        .map_err(|_| failure_message("BROKER_AGENT_FAILED", "이전 로그인 연결 작업을 실행하지 못했어요."))
}

/// 앱이 만들고 그 뒤 바뀌지 않은 이전 작업만 한 번 지운다. 소유 기록이 없으면 손대지 않는다.
/// 정의가 기록과 다르거나(사용자가 고친 작업) 지울 수 없으면(관리자 권한으로 만든 작업) 바꾸지 않고 멈춘다.
fn retire_legacy_task(paths: &Paths) -> Result<(), ApiError> {
    let path = legacy_receipt_path(paths);
    let file = match aam_protocol::secure::open_read_no_follow(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(_) => return Err(failure_message("BROKER_AGENT_CONFLICT", "이전 로그인 연결 작업 기록을 안전하게 읽지 못했어요.")),
    };
    if !aam_protocol::winutil::handle_access_is_safe(&file, true).unwrap_or(false) || file.metadata().map_or(true, |m| m.len() > 16384) {
        return Err(failure_message("BROKER_AGENT_CONFLICT", "이전 로그인 연결 작업 기록의 권한이 안전하지 않아요."));
    }
    let record: LegacyReceipt = serde_json::from_reader(file).map_err(|_| failure_message("BROKER_AGENT_CONFLICT", "이전 로그인 연결 작업 기록이 손상됐어요."))?;
    if record.owner != OWNER || record.task_name != legacy_task_name(paths) {
        return Err(failure_message("BROKER_AGENT_CONFLICT", "이 앱이 만든 로그인 연결 작업이 아니에요."));
    }
    let query = scheduler(&["/Query", "/TN", &record.task_name, "/XML"])?;
    if !query.status.success() {
        // 작업이 이미 없다. 남은 기록만 정리한다.
        let _ = fs::remove_file(&path);
        return Ok(());
    }
    if digest(&query.stdout) != record.definition_digest {
        return Err(failure_message("BROKER_AGENT_CONFLICT", "이전 로그인 연결 작업이 등록 후 바뀌어 지우지 않았어요. 작업 스케줄러에서 Ojak-OMP-Broker 작업을 직접 정리한 뒤 다시 시도해 주세요."));
    }
    if !scheduler(&["/Delete", "/TN", &record.task_name, "/F"])?.status.success() {
        return Err(failure_message("BROKER_AGENT_CONFLICT", "관리자 권한으로 만든 이전 작업이라 지울 수 없어요. 작업 스케줄러에서 Ojak-OMP-Broker 작업을 지운 뒤 다시 시도해 주세요. 작업은 그대로 뒀어요."));
    }
    let _ = fs::remove_file(&path);
    Ok(())
}

pub(super) fn ensure(paths: &Paths) -> Result<(), ApiError> {
    let runtime = require_executable(&std::env::var_os("OMP_NATIVE_BIN").map(PathBuf::from).unwrap_or_else(native_omp))?;
    retire_legacy_task(paths)?;
    if service_runtime(paths).as_ref() != Some(&runtime) {
        let bytes = serde_json::to_vec(&ServiceMarker { owner: OWNER.into(), runtime, reason: "service".into() })
            .map_err(|_| failure_message("BROKER_STATE_FAILED", "로그인 연결 감독 기록을 만들지 못했어요."))?;
        aam_protocol::secure::restrict_dir(&paths.home).map_err(|_| failure_message("BROKER_STATE_FAILED", "로그인 연결 상태 폴더를 보호하지 못했어요."))?;
        super::atomic_write(&service_marker(paths), &bytes)?;
    }
    // 서비스는 2초마다 기록을 확인한다. omp broker는 첫 기동에 30초 안팎, 길면 1분 넘게 걸린다(실측).
    let started = Instant::now();
    let deadline = started + Duration::from_secs(90);
    let mut announced = false;
    let mut noted = 0u64;
    while Instant::now() < deadline {
        if authenticated_snapshot().is_ok() { return Ok(()); }
        if !announced {
            super::cli_progress("omp broker를 기다리는 중… 첫 실행은 1분 넘게 걸릴 수 있습니다.");
            announced = true;
        }
        let elapsed = started.elapsed().as_secs();
        if elapsed >= 15 && elapsed / 15 > noted {
            noted = elapsed / 15;
            super::cli_progress(&format!("omp broker를 기다리는 중… {elapsed}초 (최대 90초)"));
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    Err(failure_message("BROKER_NOT_READY", "Ojak 서비스가 omp 로그인 연결을 켜도록 했지만 아직 응답이 없어요. 잠시 뒤 다시 시도해 주세요."))
}
