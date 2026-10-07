//! 첫 실행 준비. 앱의 준비 화면이 한 번의 동의로 서비스·명령 연결·터미널 PATH를 차례로 설치하고,
//! 끝난 뒤 실제로 `claude`·`codex`가 관리 실행으로 이어지는지 점검한다.
use crate::{install, read_snapshot, service_version};
use aam_protocol::{ApiError, Paths};
use serde::Serialize;
#[cfg(not(windows))]
use std::path::Path;
use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};

#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ToolSetup {
    pub tool: String,
    /// 이 도구로 실행 가능한 계정이 있다.
    pub account: bool,
    /// 관리하는 shim 파일이 설치돼 있다. 실제 셸 명령 확인은 verified로 구분한다.
    pub connected: bool,
    /// 명시적인 설치·재점검에서만 로그인 셸을 실행한다. 미검사는 None.
    pub verified: Option<bool>,
    pub verification_error: Option<String>,
}

/// 실패한 준비 단계의 이유와 다음 행동. `message`는 한국어이고, 화면은 `code`로 표시 언어 문장을 만든다.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SetupNotice {
    pub step: String,
    pub tool: Option<String>,
    pub code: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub params: BTreeMap<String, String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SetupStatus {
    pub service: bool,
    pub shell: bool,
    pub tools: Vec<ToolSetup>,
    pub configured: bool,
    pub omp_detected: bool,
    pub omp_supported: bool,
    /// 새 로그인 셸의 명령 연결을 확인했다. 기존 세션 전환을 의미하지 않는다.
    pub ready: bool,
    /// 설치·점검이 남긴 경고. 성공만 알리는 문장은 넣지 않는다.
    pub notices: Vec<SetupNotice>,
    /// 실행 중인 서비스 버전이 앱과 다르거나 알 수 없다(앱만 덮어쓴 경우). `aam service restart`로 맞춘다.
    pub service_version_mismatch: bool,
}

struct VerifyFailure {
    code: &'static str,
    message: String,
    params: BTreeMap<String, String>,
}

fn params(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs.iter().map(|(key, value)| ((*key).to_owned(), (*value).to_owned())).collect()
}

fn push_notice(status: &mut SetupStatus, step: &str, tool: Option<&str>, failure: &VerifyFailure) {
    if status.notices.iter().any(|notice| notice.step == step && notice.message == failure.message) {
        return;
    }
    status.notices.push(SetupNotice {
        step: step.into(),
        tool: tool.map(str::to_owned),
        code: failure.code.into(),
        message: failure.message.clone(),
        params: failure.params.clone(),
    });
}

fn notice(code: &'static str, message: String, params: BTreeMap<String, String>) -> VerifyFailure {
    VerifyFailure { code, message, params }
}

#[cfg(not(windows))]
fn manual_path_lines(bin: &Path) -> String {
    let text = bin.display().to_string();
    if text.is_empty() || text.chars().any(char::is_control) || text.contains('"') {
        return "관리 명령 폴더(앱 데이터 안의 bin)를 PATH 맨 앞에 직접 넣으세요. zsh·bash는 export PATH, fish는 fish_add_path -p 를 쓴 뒤 새 터미널을 여세요.".into();
    }
    format!("zsh·bash 시작 파일에 `export PATH=\"{text}:$PATH\"`를 넣고, fish는 config.fish에 `fish_add_path -p {text}`를 넣은 뒤 새 터미널을 여세요.")
}

#[cfg(not(windows))]
fn unsupported_shell_failure(shell: &str, bin: &Path) -> VerifyFailure {
    let shells = "/bin/zsh, /bin/bash, /bin/sh";
    let manual = manual_path_lines(bin);
    let shown = if shell.starts_with('/') && shell.len() <= 240 && !shell.chars().any(char::is_control) {
        format!("현재 셸은 {shell}입니다. ")
    } else {
        String::new()
    };
    let shell_param = if shown.is_empty() { String::new() } else { shell.to_owned() };
    let current = if shell_param.is_empty() { String::new() } else { format!(" ({shell_param})") };
    notice(
        "unsupported-shell",
        format!("{shown}이 셸은 자동 확인을 지원하지 않습니다. 지원 셸은 {shells}입니다. 관리 명령 폴더를 PATH 맨 앞에 직접 넣으세요. {manual}"),
        params(&[("shells", shells), ("manual", manual.as_str()), ("current", current.as_str())]),
    )
}

fn missing_shim_failure(tool: &str) -> VerifyFailure {
    notice(
        "missing-shim",
        format!("{tool} 명령 연결(shim)이 없습니다. [시작하기]를 다시 누르거나 터미널에서 `aam integration install`을 실행하세요."),
        params(&[("tool", tool)]),
    )
}

fn service_missing_failure() -> VerifyFailure {
    notice(
        "service-missing",
        "백그라운드 서비스가 실행 중이 아닙니다. [시작하기]를 누르면 이 사용자 계정에 서비스를 등록하고 시작합니다.".into(),
        BTreeMap::new(),
    )
}

/// 서비스가 앱보다 예전이거나 버전을 알리지 못할 때의 안내. 서비스 단계와 분리해 설치 단계를 미완료로 만들지 않는다.
fn service_version_failure(service: Option<&str>) -> VerifyFailure {
    let shown = service.unwrap_or("알 수 없음");
    notice(
        "service-version-mismatch",
        format!("실행 중인 서비스(버전 {shown})가 앱(버전 {})과 달라요. 쓰는 중인 세션이 없을 때 `aam service restart`를 실행하거나 앱의 [서비스 다시 시작]을 눌러 주세요.", service_version::APP_VERSION),
        params(&[("service", shown), ("app", service_version::APP_VERSION)]),
    )
}

/// 설치 함수가 돌려준 문장에서 경고만 준비 결과에 붙인다. 성공 안내만 있는 문장은 버린다.
fn attach_step_warning(status: &mut SetupStatus, step: &str, message: &str) {
    let Some(index) = message.find("주의:") else { return };
    let warning = message[index..].trim();
    if warning.is_empty() {
        return;
    }
    let code = if warning.contains("시스템 PATH") { "path-shadow" } else { "step-warning" };
    push_notice(status, step, None, &notice(code, warning.to_owned(), BTreeMap::new()));
}

fn include_path_shadow(status: &mut SetupStatus, tools: &[String]) {
    let Some(message) = install::path_shadow_warning(tools) else { return };
    let names = tools.join(", ");
    push_notice(status, "terminal", None, &notice("path-shadow", message, params(&[("tools", names.as_str())])));
}

pub fn setup_status(paths: &Paths) -> SetupStatus {
    let snapshot = read_snapshot(paths).ok();
    let shell = install::shell_configured(paths).unwrap_or(false);
    let tools: Vec<ToolSetup> = ["claude", "codex"]
        .into_iter()
        .map(|tool| ToolSetup {
            tool: tool.into(),
            account: snapshot.as_ref().is_some_and(|snapshot| {
                snapshot.accounts.iter().any(|a| a.tool == tool && a.enabled && a.can_launch)
            }),
            connected: install::shim_installed(paths, tool).unwrap_or(false),
            verified: None,
            verification_error: None,
        })
        .collect();
    let configured = snapshot.is_some()
        && shell
        && tools.iter().any(|t| t.account)
        && tools.iter().all(|t| !t.account || t.connected);
    let omp_detected = snapshot.as_ref().is_some_and(|snapshot| snapshot.tools.iter().any(|tool| tool.id == "omp" && tool.installed));
    let mut status = SetupStatus {
        service: snapshot.is_some(),
        shell,
        tools,
        configured,
        omp_detected,
        omp_supported: cfg!(any(unix, windows)),
        ready: false,
        notices: Vec::new(),
        service_version_mismatch: snapshot.as_ref().is_some_and(|snapshot| service_version::report_for(snapshot).mismatch),
    };
    if snapshot.is_none() {
        push_notice(&mut status, "service", None, &service_missing_failure());
    }
    if status.service_version_mismatch {
        let failure = service_version_failure(snapshot.as_ref().and_then(|snapshot| snapshot.service_version.as_deref()));
        push_notice(&mut status, "service-version", None, &failure);
    }
    let missing: Vec<String> = status.tools.iter().filter(|tool| tool.account && !tool.connected).map(|tool| tool.tool.clone()).collect();
    for tool in missing {
        let failure = missing_shim_failure(&tool);
        push_notice(&mut status, "terminal", Some(&tool), &failure);
    }
    include_path_shadow(&mut status, &install::shadowed_tools());
    status
}

/// 빠진 단계만 순서대로 설치한다. 서비스 → (첫 도구 탐지 대기) → 명령 연결 → 터미널 PATH.
/// 원본 CLI·로그인은 바꾸지 않는다. 이미 끝난 단계는 건너뛴다.
/// 각 단계가 돌려준 경고는 결과에 남긴다. 성공만 알리는 문장은 넣지 않는다.
pub fn setup_install(paths: &Paths) -> Result<SetupStatus, ApiError> {
    let mut step_messages = Vec::new();
    if read_snapshot(paths).is_err() {
        step_messages.push(("service", install::service_install(paths)?));
    }
    // 서비스가 막 시작했으면 도구·기본 프로필 탐지가 끝나야 어떤 명령을 연결할지 안다.
    let deadline = Instant::now() + Duration::from_secs(30);
    while read_snapshot(paths).map_or(true, |snapshot| snapshot.refreshing) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(300));
    }
    step_messages.push(("terminal", install::integration_install(paths)?));
    if !install::shell_configured(paths).unwrap_or(false) {
        step_messages.push(("terminal", install::shell_install(paths)?));
    }
    let mut status = setup_check(paths);
    for (step, message) in step_messages {
        attach_step_warning(&mut status, step, &message);
    }
    Ok(status)
}

/// Explicit action only: starting a login shell executes the user's startup files.
pub fn setup_check(paths: &Paths) -> SetupStatus {
    let mut status = setup_status(paths);
    include_path_shadow(&mut status, &install::shadowed_tools());
    let mut failures = Vec::new();
    for tool in &mut status.tools {
        if tool.account && tool.connected {
            match verify_command(paths, &tool.tool) {
                Ok(()) => tool.verified = Some(true),
                Err(failure) => {
                    tool.verified = Some(false);
                    tool.verification_error = Some(failure.message.clone());
                    failures.push((tool.tool.clone(), failure));
                }
            }
        }
    }
    for (tool, failure) in failures {
        push_notice(&mut status, "terminal", Some(&tool), &failure);
    }
    status.ready = status.configured
        && status.tools.iter().all(|tool| !tool.account || tool.verified == Some(true))
        && !status.notices.iter().any(|notice| notice.code == "path-shadow");
    status
}

#[cfg(unix)]
fn verify_command(paths: &Paths, tool: &str) -> Result<(), VerifyFailure> {
    use std::{os::unix::process::CommandExt, process::{Command, Stdio}};
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into());
    let bin = paths.home.join("bin");
    if !matches!(shell.as_str(), "/bin/zsh" | "/bin/bash" | "/bin/sh") {
        return Err(unsupported_shell_failure(&shell, &bin));
    }
    // Do not let this process's already-injected PATH manufacture a successful new-shell check.
    let path = std::env::join_paths(std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()).filter(|entry| entry != &bin))
        .map_err(|_| shell_error(tool))?;
    let mut child = Command::new(&shell).args(["-lic", "[ \"$(command -v \"$1\")\" -ef \"$2\" ]", "ojak-check", tool])
        .arg(bin.join(tool)).env("PATH", path).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null())
        .process_group(0).spawn().map_err(|_| shell_error(tool))?;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match child.try_wait() {
            Ok(Some(exit)) => {
                return if exit.success() { Ok(()) } else { Err(command_mismatch(tool, &bin)) };
            }
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(25)),
            _ => {
                unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL); }
                let _ = child.wait();
                return Err(shell_timeout(tool));
            }
        }
    }
}

#[cfg(unix)]
fn command_mismatch(tool: &str, bin: &Path) -> VerifyFailure {
    let expected = bin.join(tool).display().to_string();
    let manual = format!("export PATH=\"{}:$PATH\"", bin.display());
    notice(
        "command-mismatch",
        format!("{tool} 명령이 관리 shim({expected})과 다릅니다. 새 터미널에서 `command -v {tool}`가 그 경로인지 확인하세요. 자동 PATH 줄은 zsh에만 들어갑니다. bash처럼 다른 지원 셸은 시작 파일에 `{manual}`를 직접 추가한 뒤 다시 점검하세요. 지원 셸은 /bin/zsh, /bin/bash, /bin/sh입니다."),
        params(&[("tool", tool), ("path", expected.as_str()), ("manual", manual.as_str())]),
    )
}

#[cfg(unix)]
fn shell_error(tool: &str) -> VerifyFailure {
    notice(
        "shell-error",
        format!("로그인 셸을 시작하지 못해 {tool} 명령을 확인하지 못했습니다. SHELL이 실행 가능한 /bin/zsh, /bin/bash, /bin/sh인지 확인한 뒤 다시 점검하세요."),
        params(&[("tool", tool)]),
    )
}

#[cfg(unix)]
fn shell_timeout(tool: &str) -> VerifyFailure {
    notice(
        "shell-timeout",
        format!("로그인 셸이 5초 안에 끝나지 않아 {tool} 명령을 확인하지 못했습니다. 셸 시작 파일에서 기다리는 명령을 줄인 뒤 다시 점검하세요."),
        params(&[("tool", tool)]),
    )
}

/// PowerShell을 띄우지 않고 저장된 PATH를 직접 해석한다. 서명 없는 프로그램이 숨긴 PowerShell을
/// 실행하면 행위 기반 백신이 공격 패턴으로 오탐한다(Defender `Behavior:Win32/Execution.A!ml`).
#[cfg(windows)]
fn verify_command(paths: &Paths, tool: &str) -> Result<(), VerifyFailure> {
    let expected = paths.home.join("bin").join(format!("{tool}.cmd"));
    let actual = install::effective_command(tool);
    let same = |left: &std::path::Path, right: &std::path::Path| {
        std::path::absolute(left).ok().zip(std::path::absolute(right).ok())
            .is_some_and(|(left, right)| left.to_string_lossy().eq_ignore_ascii_case(&right.to_string_lossy()))
    };
    if actual.as_ref().is_some_and(|actual| same(actual, &expected)) {
        return Ok(());
    }
    let shadowed = install::shadowed_tools();
    if shadowed.iter().any(|name| name == tool) {
        if let Some(message) = install::path_shadow_warning(&shadowed) {
            let names = shadowed.join(", ");
            return Err(notice("path-shadow", message, params(&[("tools", names.as_str())])));
        }
    }
    let path = expected.display().to_string();
    Err(notice(
        "windows-mismatch",
        format!("{tool} 명령이 관리 shim({path})이 아닙니다. 사용자 PATH 맨 앞에 Ojak bin이 있는지 확인하고, 시스템 PATH에 같은 이름 설치가 있으면 제거한 뒤 새 터미널을 여세요."),
        params(&[("tool", tool), ("path", path.as_str())]),
    ))
}

#[cfg(not(any(unix, windows)))]
fn verify_command(paths: &Paths, tool: &str) -> Result<(), VerifyFailure> {
    let _ = tool;
    Err(unsupported_shell_failure("unknown", &paths.home.join("bin")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blank_status() -> SetupStatus {
        SetupStatus {
            service: true,
            shell: true,
            tools: vec![],
            configured: true,
            omp_detected: false,
            omp_supported: true,
            ready: false,
            notices: Vec::new(),
            service_version_mismatch: false,
        }
    }

    #[test]
    fn setup_result_includes_shadowed_path_warning() {
        let mut status = blank_status();
        include_path_shadow(&mut status, &["claude".into()]);
        let value = serde_json::to_value(&status).unwrap();
        let notice = &value["notices"][0];
        assert_eq!(notice["step"], "terminal");
        assert_eq!(notice["code"], "path-shadow");
        let message = notice["message"].as_str().unwrap();
        assert!(message.contains("시스템 PATH"), "{message}");
        assert!(message.contains("claude"), "{message}");
        assert!(message.contains("aam run"), "{message}");
        assert!(message.contains("새 터미널"), "{message}");
    }

    #[test]
    fn version_mismatch_is_reported_with_the_next_action() {
        let mut status = blank_status();
        status.service_version_mismatch = true;
        push_notice(&mut status, "service-version", None, &service_version_failure(None));
        let value = serde_json::to_value(&status).unwrap();
        assert_eq!(value["serviceVersionMismatch"], true);
        let notice = &value["notices"][0];
        assert_eq!(notice["code"], "service-version-mismatch");
        assert_eq!(notice["params"]["service"], "알 수 없음");
        assert!(notice["message"].as_str().unwrap().contains("aam service restart"));
    }

    #[test]
    fn install_step_warning_is_kept_on_the_setup_result() {
        let warning = install::path_shadow_warning(&["claude".into()]).unwrap();
        let returned = format!("사용자 PATH 맨 앞에 shim 폴더를 넣었습니다. 새 터미널부터 적용됩니다. {warning}");
        let mut status = blank_status();
        attach_step_warning(&mut status, "terminal", &returned);
        let message = serde_json::to_value(&status).unwrap()["notices"][0]["message"].as_str().unwrap().to_owned();
        assert!(message.contains("시스템 PATH"), "{message}");
        assert!(message.contains("claude"), "{message}");
        assert!(message.contains("aam run"), "{message}");
        assert!(!message.contains("shim 폴더를 넣었습니다"), "{message}");
    }

    #[cfg(not(windows))]
    #[test]
    fn unsupported_shell_notice_names_supported_shells_and_manual_path() {
        let bin = Path::new("/Users/example/Library/Application Support/AI Account Manager/bin");
        let failure = unsupported_shell_failure("/opt/homebrew/bin/fish", bin);
        assert_eq!(failure.code, "unsupported-shell");
        assert!(failure.message.contains("/bin/zsh"), "{}", failure.message);
        assert!(failure.message.contains("/bin/bash"), "{}", failure.message);
        assert!(failure.message.contains("/bin/sh"), "{}", failure.message);
        assert!(failure.message.contains("export PATH"), "{}", failure.message);
        assert!(failure.message.contains("fish_add_path"), "{}", failure.message);
        assert!(failure.message.contains("bin"), "{}", failure.message);
        let mut status = blank_status();
        push_notice(&mut status, "terminal", Some("claude"), &failure);
        let json = serde_json::to_string(&status).unwrap();
        assert!(json.contains("/bin/zsh"), "{json}");
        assert!(json.contains("fish_add_path"), "{json}");
    }
}
