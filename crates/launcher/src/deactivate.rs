//! Ojak 사용 중지: 사용자 환경을 Ojak을 설치하기 전 형태로 되돌린다.
//!
//! 순서가 중요하다. 첫 변경 전에 서비스의 신규 배정을 닫는다(`service.prepareUninstall`: 점유 lease가
//! 없는지 확인하고 같은 트랜잭션에서 permit을 건다). 그래서 되돌리는 도중 새 관리 세션이 끼어들 수 없다.
//! 그다음 omp가 원래 공급자와 로컬 인증 저장소를 다시 쓰도록 브릿지·broker 연결을 풀고, shell PATH와
//! shim을 걷어 낸 뒤 마지막에 관리 서비스를 내린다(같은 permit을 재사용). 중간에 실패하면 배정을 다시 연다.
//! 계정·프로필·로그와 원본 CLI·omp 로그인은 지우지 않는다.

use crate::{omp_bridge, omp_broker, omp_observer};
use aam_launcher::{install, read_snapshot};
use aam_protocol::{call, ApiError, Paths};
use serde_json::json;

const OCCUPIED: [&str; 5] = ["PREPARED", "STARTING", "ACTIVE", "SUSPECT", "ORPHANED"];

/// 되돌리는 단계와 설명. `--dry-run`과 실제 실행이 같은 목록을 쓴다.
/// broker 블록을 서비스보다 먼저 지운다. 서비스가 먼저 사라지면 omp가 죽은 broker를 계속 본다.
const STEPS: [(&str, &str); 6] = [
    ("omp-bridge", "omp /login의 Ojak 공급자 확장 제거, Ojak 모델을 가리키던 역할을 원래 공급자로 복원"),
    ("omp-broker", "omp 설정의 Ojak 로그인 연결 제거 (omp가 로컬 로그인을 다시 사용)"),
    ("omp-observer", "omp 사용량 확인 확장 제거"),
    ("shell", SHELL_STEP),
    ("integration", "Ojak 명령(aam·claude·codex) 제거"),
    ("service", SERVICE_STEP),
];
#[cfg(windows)]
const SHELL_STEP: &str = "사용자 PATH의 Ojak 항목 제거 (새 터미널부터 원래 claude·codex 실행)";
#[cfg(not(windows))]
const SHELL_STEP: &str = "zsh의 Ojak PATH 제거 (새 터미널부터 원래 claude·codex 실행)";
#[cfg(windows)]
const SERVICE_STEP: &str = "관리 서비스 중지, 로그인 시 자동 실행 제거";
#[cfg(not(windows))]
const SERVICE_STEP: &str = "관리 서비스 중지, 로그인 시 자동 실행 제거";

/// 신규 배정 차단 창구. 실제로는 서비스 RPC, 테스트에서는 기록용 가짜를 쓴다.
trait Admission {
    /// 신규 배정을 닫는다. 서비스가 꺼져 있으면 `None`(마지막 서비스 단계가 저장된 lease를 다시 확인한다).
    fn close(&mut self) -> Result<Option<String>, ApiError>;
    fn reopen(&mut self, permit: &str);
}

struct Service<'a>(&'a Paths);
impl Admission for Service<'_> {
    fn close(&mut self) -> Result<Option<String>, ApiError> {
        match call(self.0, "service.prepareUninstall", json!({})) {
            Ok(value) => value
                .get("permit")
                .and_then(|v| v.as_str())
                .filter(|v| !v.is_empty() && value.get("safe").and_then(|v| v.as_bool()) == Some(true))
                .map(|permit| Some(permit.to_owned()))
                .ok_or_else(|| ApiError::new("INVALID_UNINSTALL_PERMIT", "새 배정 차단을 확인하지 못해 아무것도 바꾸지 않았어요.")),
            Err(error) if error.code == "DAEMON_UNAVAILABLE" => Ok(None),
            Err(error) => Err(error),
        }
    }
    fn reopen(&mut self, permit: &str) {
        let _ = call(self.0, "service.cancelUninstall", json!({ "permit": permit }));
    }
}

/// 계획 확인(`--dry-run`)용 읽기 전용 점검. 실제 실행은 `Admission::close`가 원자적으로 다시 확인한다.
fn busy(paths: &Paths) -> Result<usize, ApiError> {
    match read_snapshot(paths) {
        Ok(snapshot) => Ok(snapshot.sessions.iter().filter(|session| OCCUPIED.contains(&session.state.as_str())).count()),
        Err(error) if error.code == "DAEMON_UNAVAILABLE" => Ok(0),
        Err(error) => Err(error),
    }
}

fn run_step(paths: &Paths, step: &str) -> Result<(), ApiError> {
    match step {
        "omp-bridge" => omp_bridge::disconnect(paths).map(|_| ()),
        "omp-broker" => omp_broker::disconnect(paths).map(|_| ()),
        "omp-observer" => omp_observer::run("uninstall").map(|_| ()).map_err(|_| {
            ApiError::new("OMP_OBSERVER_UNINSTALL", "사용량 확인 확장을 지우지 못했어요. 사용자 파일은 그대로 뒀어요.")
        }),
        "shell" => install::shell_uninstall(paths).map(|_| ()),
        "integration" => install::integration_uninstall(paths).map(|_| ()),
        "service" => install::service_uninstall(paths).map(|_| ()),
        _ => unreachable!("STEPS에 없는 단계"),
    }
}

pub fn run(paths: &Paths, dry_run: bool) -> Result<String, ApiError> {
    if dry_run {
        let occupied = busy(paths)?;
        let mut report = vec!["Ojak 사용 중지 계획 (아직 아무것도 바꾸지 않았습니다):".to_owned()];
        report.extend(STEPS.iter().enumerate().map(|(index, (_, text))| format!("{}. {text}", index + 1)));
        report.push("보존: 계정·프로필·로그, 원본 CLI, omp 로그인. 앱에서 다시 연결하면 원래대로 사용할 수 있습니다.".to_owned());
        if occupied > 0 {
            report.push(format!("지금은 실행 중인 관리 세션 {occupied}개가 있어 시작할 수 없습니다."));
        }
        return Ok(report.join("\n"));
    }
    execute(&mut Service(paths), |step| run_step(paths, step))
}

fn execute(admission: &mut dyn Admission, mut step: impl FnMut(&str) -> Result<(), ApiError>) -> Result<String, ApiError> {
    let permit = admission.close().map_err(|error| {
        ApiError::new(&error.code, format!("{} 아무것도 바꾸지 않았습니다.", error.message))
    })?;
    let mut report = Vec::with_capacity(STEPS.len() + 1);
    for (index, (name, text)) in STEPS.iter().enumerate() {
        if let Err(error) = step(name) {
            if let Some(permit) = &permit {
                admission.reopen(permit);
            }
            report.push(format!("✗ {text}"));
            let done = if index == 0 { "앞 단계 없음".to_owned() } else { format!("{index}단계까지 완료") };
            return Err(ApiError::new(
                &error.code,
                format!("{}\n({done}. 신규 배정은 다시 열었습니다. 원인을 해결한 뒤 다시 실행하면 이어서 진행합니다.)\n{}", error.message, report.join("\n")),
            ));
        }
        report.push(format!("✓ {text}"));
    }
    report.push("Ojak 사용을 중지했습니다. 새 터미널과 새 omp 세션부터 원래 형태로 동작합니다. 계정·프로필·로그와 omp 로그인은 보존했습니다.".to_owned());
    Ok(report.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Fake {
        busy: bool,
        log: Vec<String>,
    }
    impl Admission for Fake {
        fn close(&mut self) -> Result<Option<String>, ApiError> {
            self.log.push("close".into());
            if self.busy { Err(ApiError::new("SESSION_BUSY", "점유 lease")) } else { Ok(Some("p".into())) }
        }
        fn reopen(&mut self, permit: &str) {
            self.log.push(format!("reopen:{permit}"));
        }
    }

    #[test]
    fn admission_closes_before_any_change_and_busy_changes_nothing() {
        let mut fake = Fake { busy: true, ..Fake::default() };
        let mut steps = Vec::new();
        let error = execute(&mut fake, |step| { steps.push(step.to_owned()); Ok(()) }).unwrap_err();
        assert_eq!(error.code, "SESSION_BUSY");
        assert!(steps.is_empty());

        let mut fake = Fake::default();
        let mut order = Vec::new();
        execute(&mut fake, |step| { order.push(step.to_owned()); Ok(()) }).unwrap();
        assert_eq!(fake.log, vec!["close"]);
        assert_eq!(order, STEPS.map(|(name, _)| name.to_owned()));
    }

    #[test]
    fn failure_midway_stops_and_reopens_admission() {
        let mut fake = Fake::default();
        let mut ran = Vec::new();
        let error = execute(&mut fake, |step| {
            ran.push(step.to_owned());
            if step == "shell" { Err(ApiError::new("SHELL_CONFLICT", "zshrc 변경됨")) } else { Ok(()) }
        })
        .unwrap_err();
        assert_eq!(error.code, "SHELL_CONFLICT");
        assert_eq!(ran, vec!["omp-bridge", "omp-broker", "omp-observer", "shell"]);
        assert_eq!(fake.log, vec!["close", "reopen:p"]);
    }
}
