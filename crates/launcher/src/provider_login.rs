//! `aam provider-login`: 앱이 시작하는 공식 로그인 작업 하나를 끝까지 책임진다.
//!
//! - Claude·Codex: 공식 CLI의 로그인(`claude auth login`, `codex login`)을 격리 프로필에서 연다. 새 계정이나
//!   만료된 omp 전용 계정은 새 관리 프로필에, 이미 등록된 native 계정은 그 프로필에 로그인한다. 로그인 뒤 공식 CLI로
//!   계정·워크스페이스를 확인하고, 선택한 계정과 같을 때만 서비스에 등록한다.
//! - xAI(`xai-oauth`)·Gemini(`google-antigravity`): `omp --mode rpc --no-ui`의 공식 OAuth `login` 명령을 쓴다.
//!   omp가 알려 주는 인증 주소(허용 호스트만)를 앱이 브라우저로 열고, 끝나면 omp 인증 저장소(broker)의 새 항목으로
//!   계정을 확인한다. 토큰·인증 주소·omp 원문 오류는 출력하지 않는다.
//!
//! 출력(stdout)은 한 줄에 JSON 하나이며 매번 전체 상태다. 끝나는 줄은 정확히 하나(succeeded|failed|canceled)이고
//! 종료 코드는 0/1/130이다. stdin이 닫히거나 `{"type":"cancel"}`을 받거나 SIGTERM이 오면 취소한다:
//! 이 작업이 시작한 프로세스 그룹(Windows는 Job)만 종료하고, 이 작업이 만든 미등록 프로필만 지운다.

use crate::{omp_bridge, omp_broker};
use aam_launcher::{extend_integration, install, read_snapshot};
use aam_protocol::{call, now_ms, Account, ApiError, Paths};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    sync::{atomic::{AtomicBool, Ordering}, mpsc},
    thread,
    time::{Duration, Instant},
};

const LOGIN_TIMEOUT: Duration = Duration::from_secs(600);
const SYNC_TIMEOUT: Duration = Duration::from_secs(150);
const OMP_READY_TIMEOUT: Duration = Duration::from_secs(30);
const STOP_GRACE: Duration = Duration::from_secs(3);

static CANCEL: AtomicBool = AtomicBool::new(false);

pub struct Request {
    pub provider: String,
    pub account: Option<String>,
    pub label: Option<String>,
    pub settings_digest: Option<String>,
}

struct Fail {
    code: String,
    message: String,
    retryable: bool,
}
enum Stop {
    Canceled,
    Failed(Fail),
}
type R<T> = Result<T, Stop>;

impl From<ApiError> for Stop {
    fn from(error: ApiError) -> Self {
        Stop::Failed(Fail { code: error.code, message: error.message, retryable: error.retryable })
    }
}
fn fail(code: &str, message: &str, retryable: bool) -> Stop {
    Stop::Failed(Fail { code: code.into(), message: message.into(), retryable })
}

struct Done {
    account_id: String,
    sync: &'static str,
}

/// 진행 상태를 JSON 줄로 알린다. 끝나는 줄은 한 번만 낸다. 읽는 쪽이 사라지면(쓰기 실패) 취소로 본다.
#[derive(Default)]
struct Emitter {
    identity: Option<Value>,
    committed: Option<String>,
    terminal: bool,
}
impl Emitter {
    fn line(&mut self, state: &str, error: Option<&Fail>, sync: Option<&str>) {
        if self.terminal {
            return;
        }
        self.terminal = matches!(state, "succeeded" | "failed" | "canceled");
        let value = json!({
            "state": state,
            "accountId": self.committed,
            "identity": self.identity,
            "error": error.map(|e| json!({"code": e.code, "message": e.message, "retryable": e.retryable})),
            "sync": sync,
        });
        let mut out = std::io::stdout().lock();
        if writeln!(out, "{value}").and_then(|_| out.flush()).is_err() {
            CANCEL.store(true, Ordering::Relaxed);
        }
    }
    fn state(&mut self, state: &str) {
        self.line(state, None, None);
    }
}

struct Ctx<'a> {
    paths: &'a Paths,
    emitter: Emitter,
    deadline: Instant,
}
impl Ctx<'_> {
    fn cancelled(&self) -> R<()> {
        if CANCEL.load(Ordering::Relaxed) {
            return Err(Stop::Canceled);
        }
        Ok(())
    }
    fn check(&self) -> R<()> {
        self.cancelled()?;
        if Instant::now() >= self.deadline {
            return Err(fail("LOGIN_TIMEOUT", "10분 안에 로그인이 끝나지 않아 멈췄어요. 다시 시도해 주세요.", true));
        }
        Ok(())
    }
}

pub fn run(paths: &Paths, request: Request) -> i32 {
    #[cfg(unix)]
    install_signals();
    watch_stdin();
    let mut ctx = Ctx { paths, emitter: Emitter::default(), deadline: Instant::now() + LOGIN_TIMEOUT };
    ctx.emitter.state("starting");
    match execute(&mut ctx, &request) {
        Ok(done) => {
            ctx.emitter.committed = Some(done.account_id);
            ctx.emitter.line("succeeded", None, Some(done.sync));
            0
        }
        Err(Stop::Canceled) => {
            ctx.emitter.state("canceled");
            130
        }
        Err(Stop::Failed(error)) => {
            ctx.emitter.line("failed", Some(&error), None);
            1
        }
    }
}

fn execute(ctx: &mut Ctx, request: &Request) -> R<Done> {
    if !cfg!(any(target_os = "macos", windows)) {
        return Err(fail("PLATFORM_UNSUPPORTED", "앱의 공식 로그인은 macOS와 Windows에서만 지원해요.", false));
    }
    if let Some(id) = &request.account {
        if id.is_empty() || id.len() > 128 || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') {
            return Err(fail("INVALID_PARAMS", "다시 로그인할 계정을 확인하지 못했어요.", false));
        }
    }
    match request.provider.as_str() {
        "anthropic" => native(ctx, "claude", request),
        "openai-codex" => native(ctx, "codex", request),
        "xai-oauth" | "google-antigravity" => omp_login(ctx, request),
        _ => Err(fail("ADAPTER_UNVERIFIED", "지원하지 않는 공급자예요.", false)),
    }
}

// ---------------------------------------------------------------------------------------------
// 취소 신호

#[cfg(unix)]
extern "C" fn on_signal(_: libc::c_int) {
    CANCEL.store(true, Ordering::Relaxed);
}
#[cfg(unix)]
fn install_signals() {
    unsafe {
        for signal in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
            libc::signal(signal, on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t);
        }
        // 앱이 사라졌을 때 쓰기 실패로 죽어 자식을 남기지 않는다. 쓰기 실패는 취소로 처리한다.
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }
}

fn is_cancel_line(line: &str) -> bool {
    serde_json::from_str::<Value>(line.trim()).ok().and_then(|value| value["type"].as_str().map(|kind| kind == "cancel")).unwrap_or(false)
}

/// stdin 닫힘(앱 종료·충돌 포함)과 취소 명령을 취소로 본다.
fn watch_stdin() {
    thread::spawn(|| {
        let stdin = std::io::stdin();
        let mut line = String::new();
        loop {
            line.clear();
            match stdin.lock().read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) if is_cancel_line(&line) => break,
                Ok(_) => {}
            }
        }
        CANCEL.store(true, Ordering::Relaxed);
    });
}

// ---------------------------------------------------------------------------------------------
// 소유한 자식 프로세스

/// 이 작업이 시작한 프로세스 하나와 그 자손. Unix는 새 프로세스 그룹, Windows는 kill-on-close Job으로 묶는다.
/// 버려질 때(취소·실패·panic) 반드시 정리한다.
struct Owned {
    child: Child,
    /// 한 번 정리한 뒤에는 PGID가 다른 프로세스에 재사용될 수 있으므로 다시 신호를 보내지 않는다.
    stopped: bool,
    #[cfg(unix)]
    pgid: i32,
    #[cfg(windows)]
    job: Option<aam_protocol::ProcessJob>,
}
impl Owned {
    fn spawn(mut command: Command) -> Result<Self, ApiError> {
        let failed = || ApiError::new("SPAWN_FAILED", "공식 로그인 프로그램을 시작하지 못했어요. 설치와 실행 권한을 확인해 주세요.");
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
            let child = command.spawn().map_err(|_| failed())?;
            let pgid = child.id() as i32;
            Ok(Self { child, stopped: false, pgid })
        }
        #[cfg(windows)]
        {
            let (child, job) = aam_protocol::spawn_in_job(&mut command, true, false, false).map_err(|_| failed())?;
            Ok(Self { child, stopped: false, job: Some(job) })
        }
    }

    fn exited(&mut self) -> Option<ExitStatus> {
        self.child.try_wait().ok().flatten()
    }

    #[cfg(unix)]
    fn group_alive(&self) -> bool {
        // 그룹이 남아 있는 동안 PGID는 다른 프로세스에 재사용되지 않는다.
        unsafe { libc::kill(-self.pgid, 0) == 0 || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH) }
    }

    /// 이 작업의 프로세스 그룹만 종료한다: SIGTERM → 유예 → SIGKILL. 이미 끝난 뒤 남은 구성원도 같은 규칙으로 치운다.
    fn stop(&mut self) {
        if std::mem::replace(&mut self.stopped, true) {
            return;
        }
        #[cfg(unix)]
        {
            if self.group_alive() {
                unsafe { libc::kill(-self.pgid, libc::SIGTERM) };
                let end = Instant::now() + STOP_GRACE;
                while Instant::now() < end && (self.child.try_wait().ok().flatten().is_none() || self.group_alive()) {
                    thread::sleep(Duration::from_millis(50));
                }
                if self.group_alive() {
                    unsafe { libc::kill(-self.pgid, libc::SIGKILL) };
                }
            }
            let _ = self.child.wait();
        }
        #[cfg(windows)]
        {
            self.job.take();
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}
impl Drop for Owned {
    fn drop(&mut self) {
        self.stop();
    }
}

/// 이 작업이 새로 만든(아직 등록하지 않은) 프로필. 등록 전에 끝나면 지운다.
struct NewProfile {
    path: Option<PathBuf>,
}
impl NewProfile {
    fn keep(&mut self) {
        self.path = None;
    }
}
impl Drop for NewProfile {
    fn drop(&mut self) {
        if let Some(path) = self.path.take() {
            let _ = std::fs::remove_dir_all(path);
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Claude·Codex: 공식 CLI 로그인

fn clean_label(label: &str) -> String {
    let trimmed = label.trim();
    if trimmed.is_empty() || trimmed.chars().any(char::is_control) {
        "계정".into()
    } else if trimmed.chars().count() > 120 {
        trimmed.chars().take(40).collect()
    } else {
        trimmed.to_owned()
    }
}

fn managed_label(tool: &str, target: &Account) -> String {
    let name = if tool == "claude" { "Claude" } else { "Codex" };
    match target.email.as_deref().map(str::trim).filter(|email| !email.is_empty() && email.len() <= 80 && !email.chars().any(char::is_control)) {
        Some(email) => format!("{name} · {email}"),
        None => clean_label(&target.label).chars().take(40).collect(),
    }
}

/// omp 관측 계정(`Account.provider`)에서 쓰는 이 도구의 공급자 계열 이름.
fn omp_family(tool: &str) -> &'static str {
    if tool == "claude" { "anthropic" } else { "openai" }
}

fn provider_of(tool: &str) -> &'static str {
    if tool == "claude" { "anthropic" } else { "openai-codex" }
}

pub(crate) fn mask_email(text: &str) -> String {
    let mut chars = text.chars();
    let first = chars.next();
    match (first, text.split_once('@')) {
        (Some(first), Some((_, domain))) if !domain.is_empty() => format!("{first}***@{domain}"),
        (Some(first), _) => format!("{first}***"),
        _ => "계정".into(),
    }
}

fn workspace_label(value: Option<&str>) -> Option<String> {
    // 식별자(UUID)는 사용자에게 의미가 없으므로 이름처럼 보이는 값만 알린다.
    value
        .map(str::trim)
        .filter(|text| !text.is_empty() && text.len() <= 60 && !(text.len() == 36 && text.matches('-').count() == 4) && !text.chars().any(char::is_control))
        .map(str::to_owned)
}

fn announce(ctx: &mut Ctx, email: Option<&str>, workspace: Option<&str>) {
    ctx.emitter.identity = Some(json!({
        "label": email.map(mask_email).unwrap_or_else(|| "계정".into()),
        "workspace": workspace_label(workspace),
    }));
}

fn native(ctx: &mut Ctx, tool: &str, request: &Request) -> R<Done> {
    let paths = ctx.paths;
    // 계정을 만들기 전에 서비스 접근부터 확인한다. 기존 로그인 파일은 복제하지 않는다.
    let snapshot = read_snapshot(paths)?;
    let target = match &request.account {
        Some(id) => Some(
            snapshot.accounts.iter().find(|account| &account.id == id).cloned()
                .ok_or_else(|| fail("ACCOUNT_NOT_FOUND", "다시 로그인할 계정을 찾지 못했어요.", false))?,
        ),
        None => None,
    };
    let mut created = NewProfile { path: None };
    let plan = match &target {
        Some(target) if target.tool == tool => {
            let profile = target.profile_path.clone()
                .ok_or_else(|| fail("PROFILE_UNBOUND", "이 계정에 연결된 프로필이 없어 다시 로그인할 수 없어요.", false))?;
            aam_adapters::relogin_plan(paths, tool, &clean_label(&target.label), Path::new(&profile))?
        }
        // 만료된 omp 전용 계정: omp 계정 ID는 native ID가 아니다. 새 관리 프로필에 공식 로그인하고 identity로만 대조한다.
        Some(target) if target.tool == "omp" && target.provider == omp_family(tool) => {
            let plan = aam_adapters::enrollment_plan(paths, tool, &managed_label(tool, target))?;
            created.path = Some(PathBuf::from(&plan.profile_path));
            plan
        }
        Some(_) => return Err(fail("INVALID_PARAMS", "이 계정은 선택한 공급자로 다시 로그인할 수 없어요.", false)),
        None => {
            let label = request.label.as_deref().ok_or_else(|| fail("LABEL_INVALID", "계정 이름을 입력해 주세요.", false))?;
            let settings = request.settings_digest.as_deref().map(|digest| aam_adapters::prepare_settings_import(tool, digest)).transpose()?;
            let plan = aam_adapters::enrollment_plan(paths, tool, label)?;
            created.path = Some(PathBuf::from(&plan.profile_path));
            if let Some(settings) = settings {
                aam_adapters::apply_settings_import(Path::new(&plan.profile_path), settings)?;
            }
            plan
        }
    };
    ctx.check()?;
    let program = install::native_program(Path::new(&plan.program))?;
    let mut command = Command::new(program);
    command
        .env_remove("CLAUDE_CONFIG_DIR")
        .args(&plan.args)
        .envs(&plan.env)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut official = Owned::spawn(command)?;
    ctx.emitter.state("waiting");
    let status = loop {
        if let Some(status) = official.exited() {
            break status;
        }
        ctx.check()?;
        thread::sleep(Duration::from_millis(100));
    };
    // 로그인 서버 같은 남은 자손도 끝났는지 확인하고 치운다.
    official.stop();
    if !status.success() {
        return Err(fail("LOGIN_FAILED", "공식 로그인이 끝나지 않았어요. 다시 시도해 주세요.", true));
    }
    ctx.emitter.state("verifying");
    // 등록(저장)하지 않고 공식 CLI로 계정부터 확인한다.
    let inspected = aam_adapters::register(paths, tool, &plan.label, &plan.profile_path)?;
    announce(ctx, inspected.email.as_deref(), inspected.organization.as_deref());
    match &target {
        Some(target) if target.tool == tool => {
            if target.identity_key.is_some() && target.identity_key != inspected.identity_key {
                return Err(mismatch());
            }
        }
        Some(target) => {
            if !aam_adapters::same_login(&inspected, target) {
                return Err(mismatch());
            }
        }
        None => {}
    }
    ctx.check()?;
    let registered: Account = serde_json::from_value(call(
        paths,
        "account.register",
        json!({"tool": tool, "label": plan.label, "profilePath": plan.profile_path}),
    )?)
    .map_err(|_| fail("PROTOCOL_MISMATCH", "관리 서비스 응답을 읽지 못했어요.", true))?;
    // 여기부터는 등록된 프로필이다. 취소해도 지우지 않는다.
    created.keep();
    ctx.emitter.committed = Some(registered.id.clone());
    let _ = extend_integration(paths, tool);
    ctx.emitter.state("syncing");
    // 서비스의 native 게이트웨이 identity(`bridge_native::identity`)와 같은 규칙: 이메일과 공식 워크스페이스가 둘 다 있어야 한다.
    // 워크스페이스를 확인하지 못한 계정은 브릿지가 게이트웨이를 만들지 않으므로 브릿지가 켜져 있으면 OMP_SYNC_UNAVAILABLE로 끝낸다.
    let gateway = registered
        .email
        .as_deref()
        .filter(|email| !email.is_empty())
        .zip(
            registered
                .identity_key
                .as_deref()
                .and_then(|key| key.split('|').find_map(|part| part.strip_prefix("workspace:")))
                .filter(|workspace| !workspace.is_empty()),
        )
        .map(|(email, workspace)| (provider_of(tool), account_key(&format!("email:{}|org:{}", email.to_lowercase(), workspace.to_lowercase()))));
    let id = registered.id.clone();
    // 공식 CLI로 다시 확인된(preflight-verified) 인증·실행 가능 상태만 인정한다. error·unverified·실행 불가는 반영으로 보지 않는다.
    let sync = wait_synced(
        ctx,
        &|account: &Account| {
            account.id == id && account.auth_status == "authenticated" && account.can_launch && account.verification == "preflight-verified"
        },
        gateway,
    )?;
    Ok(Done { account_id: registered.id, sync })
}

fn mismatch() -> Stop {
    fail(
        "ACCOUNT_MISMATCH",
        "로그인한 계정이 선택한 계정이나 워크스페이스와 달라요. 등록하지 않았어요. 원래 계정으로 다시 로그인해 주세요.",
        true,
    )
}

// ---------------------------------------------------------------------------------------------
// xAI·Gemini: omp의 공식 OAuth login

fn upstream_provider(provider: &str) -> &'static str {
    match provider {
        "xai-oauth" => "xai-oauth",
        _ => "google-antigravity",
    }
}

/// omp가 알려 준 인증 주소를 앱이 열어도 되는지. https와 공급자 허용 호스트(포트·사용자 정보 없음)만 허용한다.
fn allowed_auth_url(provider: &str, url: &str) -> bool {
    if url.len() > 4096 || url.chars().any(|c| c.is_control() || c.is_whitespace() || c == '\\') {
        return false;
    }
    let Some(rest) = url.strip_prefix("https://") else { return false };
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..end];
    if authority.is_empty() || authority.contains('@') || authority.contains(':') {
        return false;
    }
    let host = authority.to_ascii_lowercase();
    let allowed: &[&str] = match provider {
        "xai-oauth" => &["x.ai", "grok.com"],
        "google-antigravity" => &["accounts.google.com"],
        _ => &[],
    };
    allowed.iter().any(|domain| host == *domain || host.ends_with(&format!(".{domain}")))
}

fn open_browser(url: &str) -> bool {
    #[cfg(target_os = "macos")]
    {
        Command::new("/usr/bin/open").arg(url).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|status| status.success())
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        Command::new("rundll32").args(["url.dll,FileProtocolHandler", url]).creation_flags(0x0800_0000).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|status| status.success())
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        let _ = url;
        false
    }
}

fn omp_login(ctx: &mut Ctx, request: &Request) -> R<Done> {
    let paths = ctx.paths;
    let provider = request.provider.as_str();
    let upstream = upstream_provider(provider);
    let snapshot = read_snapshot(paths)?;
    let target = match &request.account {
        Some(id) => {
            let account = snapshot.accounts.iter().find(|account| &account.id == id).cloned()
                .ok_or_else(|| fail("ACCOUNT_NOT_FOUND", "다시 로그인할 계정을 찾지 못했어요.", false))?;
            let expected = if provider == "xai-oauth" { "xai" } else { "google" };
            if account.tool != "omp" || account.provider != expected {
                return Err(fail("INVALID_PARAMS", "이 계정은 선택한 공급자로 다시 로그인할 수 없어요.", false));
            }
            Some(account)
        }
        None => None,
    };
    let started = now_ms();
    let program = omp_bridge::omp()?;
    std::fs::create_dir_all(&paths.home).map_err(|_| fail("PATH_ERROR", "앱 폴더를 만들지 못했어요.", false))?;
    let mut command = Command::new(program);
    command
        .args(["--mode", "rpc", "--no-ui", "--no-session", "--no-extensions", "--no-skills", "--no-rules", "--no-lsp", "--no-tools", "--no-title"])
        .current_dir(&paths.home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut omp = Owned::spawn(command)?;
    let mut stdin = omp.child.stdin.take().ok_or_else(|| fail("SPAWN_FAILED", "omp 로그인을 시작하지 못했어요.", true))?;
    let stdout = omp.child.stdout.take().ok_or_else(|| fail("SPAWN_FAILED", "omp 로그인을 시작하지 못했어요.", true))?;
    let (frames, incoming) = mpsc::channel::<String>();
    thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            match line {
                Ok(line) => {
                    if frames.send(line).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });
    let began = Instant::now();
    let mut ready = false;
    loop {
        ctx.check()?;
        if omp.exited().is_some() {
            return Err(fail("LOGIN_FAILED", "omp 로그인이 중간에 끝났어요. 다시 시도해 주세요.", true));
        }
        let line = match incoming.recv_timeout(Duration::from_millis(200)) {
            Ok(line) => line,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if !ready && began.elapsed() > OMP_READY_TIMEOUT {
                    return Err(fail("LOGIN_FAILED", "omp 로그인을 시작하지 못했어요. omp 설치를 확인해 주세요.", true));
                }
                continue;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(fail("LOGIN_FAILED", "omp 로그인이 중간에 끝났어요. 다시 시도해 주세요.", true));
            }
        };
        let Ok(frame) = serde_json::from_str::<Value>(&line) else { continue };
        match frame["type"].as_str() {
            Some("ready") if !ready => {
                ready = true;
                let command = json!({"id": "login", "type": "login", "providerId": upstream});
                if writeln!(stdin, "{command}").and_then(|_| stdin.flush()).is_err() {
                    return Err(fail("LOGIN_FAILED", "omp 로그인을 시작하지 못했어요.", true));
                }
            }
            Some("extension_ui_request") if frame["method"] == "open_url" => {
                let url = frame["url"].as_str().unwrap_or("");
                if !allowed_auth_url(provider, url) {
                    return Err(fail("LOGIN_FAILED", "예상하지 못한 로그인 주소라 열지 않았어요.", false));
                }
                if !open_browser(url) {
                    return Err(fail("BROWSER_OPEN_FAILED", "브라우저를 열지 못했어요. 기본 브라우저 설정을 확인해 주세요.", true));
                }
                ctx.emitter.state("waiting");
            }
            Some("response") if frame["id"] == "login" => {
                if frame["success"] == true {
                    break;
                }
                // omp 원문 오류에는 주소·코드가 들어 있을 수 있어 알리지 않는다.
                return Err(fail("LOGIN_FAILED", "로그인이 완료되지 않았어요. 다시 시도해 주세요.", true));
            }
            _ => {}
        }
    }
    drop(stdin);
    omp.stop();
    ctx.emitter.state("verifying");
    let credentials = omp_broker::credentials().map_err(|error| {
        Stop::Failed(Fail { code: "OMP_LOGIN_UNVERIFIED".into(), message: format!("{} 로그인은 끝났지만 계정을 확인하지 못했어요.", error.message), retryable: true })
    })?;
    let mut found: Vec<(String, &omp_broker::Credential)> = credentials
        .iter()
        .filter(|credential| credential.provider == upstream && credential.authorized_at >= started - 5_000)
        .filter_map(|credential| {
            aam_adapters::omp_credential_identity(upstream, credential.email.as_deref(), credential.account_id.as_deref(), credential.org_id.as_deref(), credential.project_id.as_deref())
                .map(|identity| (identity, credential))
        })
        .collect();
    found.sort_by(|a, b| a.0.cmp(&b.0));
    found.dedup_by(|a, b| a.0 == b.0);
    let [(identity, credential)] = found.as_slice() else {
        return Err(fail("OMP_LOGIN_UNVERIFIED", "새 로그인의 계정을 하나로 확인하지 못했어요. 다시 시도해 주세요.", true));
    };
    announce(ctx, credential.email.as_deref(), None);
    if let Some(target) = &target {
        if target.identity_key.as_deref() != Some(identity.as_str()) {
            return Err(mismatch());
        }
    }
    ctx.cancelled()?;
    ctx.emitter.state("syncing");
    let gateway = credential.identity_key.as_deref().map(|key| (upstream, account_key(key)));
    let expected = identity.clone();
    let id = std::sync::Mutex::new(String::new());
    // omp가 이번 로그인 이후에 실제로 읽은 한도(OMP usage 버킷, known|exhausted)가 있는 같은 identity 관측만 인정한다.
    let sync = wait_synced(
        ctx,
        &|account: &Account| {
            let hit = account.tool == "omp"
                && account.identity_key.as_deref() == Some(expected.as_str())
                && !matches!(account.auth_status.as_str(), "auth-required" | "error")
                && account.buckets.iter().any(|bucket| {
                    bucket.source.starts_with("OMP usage")
                        && matches!(bucket.status.as_str(), "known" | "exhausted")
                        && bucket.observed_at >= started
                });
            if hit {
                *id.lock().unwrap_or_else(|e| e.into_inner()) = account.id.clone();
            }
            hit
        },
        gateway,
    )?;
    let account_id = id.into_inner().unwrap_or_else(|e| e.into_inner());
    ctx.emitter.committed = Some(account_id.clone());
    Ok(Done { account_id, sync })
}

// ---------------------------------------------------------------------------------------------
// 서비스·브릿지 반영 확인

/// 브릿지 로그에 쓰는 계정 칸과 같은 값(identity sha256 앞 12 hex).
fn account_key(identity: &str) -> String {
    Sha256::digest(identity.as_bytes()).iter().take(6).fold(String::new(), |mut out, byte| {
        out.push_str(&format!("{byte:02x}"));
        out
    })
}

enum Bridge {
    Disabled,
    Ready,
    Waiting,
}

fn bridge_state(paths: &Paths, gateway: &Option<(&str, String)>) -> Bridge {
    let Ok(status) = call(paths, "bridge.status", json!({})) else { return Bridge::Waiting };
    if status["enabled"] != true {
        return Bridge::Disabled;
    }
    let Some((provider, key)) = gateway else { return Bridge::Waiting };
    let ready = status["gateways"].as_array().is_some_and(|gateways| {
        gateways.iter().any(|gateway| gateway["provider"] == *provider && gateway["accountKey"] == key.as_str() && gateway["running"] == true)
    });
    if ready { Bridge::Ready } else { Bridge::Waiting }
}

/// 서비스가 계정을 새 관측으로 보이게 하고(할 수 있다면) omp `ojak-*` 게이트웨이까지 올라올 때까지 기다린다.
/// 성공은 반영을 확인했을 때(`synced`)나 브릿지를 꺼 둔 경우(`disabled`)뿐이다. 오래 걸리면 실패로 끝낸다.
fn wait_synced(ctx: &mut Ctx, expected: &dyn Fn(&Account) -> bool, gateway: Option<(&str, String)>) -> R<&'static str> {
    let paths = ctx.paths;
    if gateway.is_none() && call(paths, "bridge.status", json!({})).is_ok_and(|status| status["enabled"] == true) {
        return Err(fail("OMP_SYNC_UNAVAILABLE", "로그인은 저장됐지만 이 계정을 omp 모델에 연결할 식별 정보가 부족해요.", false));
    }
    let since = now_ms();
    let end = Instant::now() + SYNC_TIMEOUT;
    // 새 로그인은 omp 캐시에 없으므로 한 번만 강제 조회한다. 15초마다 다시 부르면 공급자 `/usage`가 반복 호출된다.
    // 진행 중인 조회가 있어 무시됐다면, 그것이 끝난 뒤 한 번 더 요청한다.
    let mut requested = false;
    loop {
        ctx.cancelled()?;
        if Instant::now() >= end {
            return Err(fail("LOGIN_SYNC_TIMEOUT", "로그인은 저장됐지만 omp 모델 연결을 아직 확인하지 못했어요. 잠시 뒤 계정 화면에서 확인해 주세요.", true));
        }
        if !requested {
            requested = call(paths, "quota.refresh", json!({"force": true})).is_ok_and(|snapshot| snapshot["refreshing"] == true);
        }
        if let Ok(snapshot) = read_snapshot(paths) {
            let fresh = snapshot.last_refresh_at.is_some_and(|at| at >= since) && !snapshot.refreshing;
            if fresh && snapshot.accounts.iter().any(|account| expected(account)) {
                match bridge_state(paths, &gateway) {
                    Bridge::Ready => return Ok("synced"),
                    Bridge::Disabled => return Ok("disabled"),
                    Bridge::Waiting => {}
                }
            }
        }
        thread::sleep(Duration::from_secs(1));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_urls_are_limited_to_the_providers_https_hosts() {
        assert!(allowed_auth_url("xai-oauth", "https://auth.x.ai/oauth2/auth?client_id=a&state=b"));
        assert!(allowed_auth_url("xai-oauth", "https://accounts.x.ai/sign-in"));
        assert!(allowed_auth_url("google-antigravity", "https://accounts.google.com/o/oauth2/v2/auth?x=1"));
        for bad in [
            "http://auth.x.ai/",
            "https://evil.example/?next=https://auth.x.ai/",
            "https://auth.x.ai.evil.example/",
            "https://notx.ai/",
            "https://user@auth.x.ai/",
            "https://auth.x.ai:8443/",
            "https://auth.x.ai\\@evil.example/",
            "https://auth.x.ai/ \n",
            "file:///etc/passwd",
            "https://",
        ] {
            assert!(!allowed_auth_url("xai-oauth", bad), "{bad}");
        }
        assert!(!allowed_auth_url("google-antigravity", "https://auth.x.ai/"));
        assert!(!allowed_auth_url("anthropic", "https://claude.ai/"));
        assert!(!allowed_auth_url("xai-oauth", &format!("https://auth.x.ai/{}", "a".repeat(5000))));
    }

    #[test]
    fn cancel_is_stdin_eof_or_an_explicit_command() {
        assert!(is_cancel_line("{\"type\":\"cancel\"}\n"));
        assert!(!is_cancel_line("{\"type\":\"ping\"}"));
        assert!(!is_cancel_line("garbage"));
    }

    #[test]
    fn identity_labels_never_reveal_the_address() {
        assert_eq!(mask_email("person@example.com"), "p***@example.com");
        assert_eq!(mask_email("noemail"), "n***");
        assert_eq!(workspace_label(Some("0f8fad5b-d9cb-469f-a165-70867728950e")), None);
        assert_eq!(workspace_label(Some("pro")).as_deref(), Some("pro"));
    }

    #[test]
    fn account_key_matches_the_bridge_log_column() {
        let key = account_key("email:a@x.com|org:o");
        assert_eq!(key.len(), 12);
        assert!(key.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_eq!(key, account_key("email:a@x.com|org:o"));
    }

    #[cfg(unix)]
    #[test]
    fn stopping_an_owned_process_kills_its_whole_group_and_nothing_else() {
        let bystander = {
            let mut command = Command::new("/bin/sleep");
            command.arg("30");
            Owned::spawn(command).unwrap()
        };
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "sleep 30 & sleep 30"]);
        let mut owned = Owned::spawn(command).unwrap();
        thread::sleep(Duration::from_millis(200));
        assert!(owned.group_alive());
        owned.stop();
        assert!(!owned.group_alive());
        let mut bystander = bystander;
        assert!(bystander.exited().is_none());
    }

    #[test]
    fn unregistered_profiles_are_removed_and_registered_ones_are_kept() {
        let dir = std::env::temp_dir().join(format!("aam-login-{}", aam_protocol::new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        drop(NewProfile { path: Some(dir.clone()) });
        assert!(!dir.exists());
        std::fs::create_dir_all(&dir).unwrap();
        let mut kept = NewProfile { path: Some(dir.clone()) };
        kept.keep();
        drop(kept);
        assert!(dir.exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
