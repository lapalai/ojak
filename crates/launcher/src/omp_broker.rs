use aam_protocol::{ApiError, Paths};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{fs::{self, OpenOptions}, io::{Read, Write}, net::TcpStream, path::{Path, PathBuf}, time::Duration};
#[cfg(unix)]
use std::{os::unix::fs::PermissionsExt, process::{Command, Stdio}, time::Instant};
#[cfg(windows)]
#[path = "omp_broker_windows.rs"]
mod windows;

const BROKER_URL: &str = "http://127.0.0.1:8765";
const BROKER_PORT: u16 = 8765;
const CONFIG_BEGIN: &str = "# >>> ai-account-manager omp broker >>>";
const CONFIG_END: &str = "# <<< ai-account-manager omp broker <<<";
#[cfg(unix)]
const AGENT_LABEL: &str = "ai.aam.omp-broker";

/// broker는 기본 프로필의 기존 인증 저장소를 그대로 서빙한다.
/// 계정을 복사하지 않으므로 재로그인도, 중복 refresh 정본도 발생하지 않는다.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    pub supported: bool,
    pub connected: bool,
    pub config_path: String,
    pub token_path: String,
    pub url: String,
    pub managed_block: bool,
    pub source_digest: String,
    /// launchd가 broker를 감독 중인지. 재부팅 후 자동 복구 여부를 뜻한다.
    pub supervised: bool,
    /// 인증된 조회로 확인한 사용 가능한 계정 수. 서버 정상과 구분한다.
    pub account_count: Option<usize>,
    /// 계정 provider 목록. token 등 비밀은 포함하지 않는다.
    pub providers: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Receipt { applied_digest: String }

#[cfg(unix)]
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AgentReceipt { plist_digest: String }

/// snapshot에서 비밀을 제외하고 필요한 metadata만 추출한다.
struct Snapshot { accounts: usize, providers: Vec<String> }

fn home() -> Result<PathBuf, ApiError> {
    aam_protocol::user_home().filter(|p| p.is_absolute()).ok_or_else(|| ApiError::new("HOME_UNAVAILABLE", "사용자 홈 경로를 확인하지 못했어요."))
}
fn config_path() -> Result<PathBuf, ApiError> { Ok(home()?.join(".omp/agent/config.yml")) }
fn yaml_path() -> Result<PathBuf, ApiError> { Ok(home()?.join(".omp/agent/config.yaml")) }
fn token_path() -> Result<PathBuf, ApiError> { Ok(home()?.join(".omp/auth-broker.token")) }
fn receipt_path(paths: &Paths) -> PathBuf { paths.home.join("omp-broker.receipt.json") }
fn native_omp() -> PathBuf {
    #[cfg(windows)]
    { std::env::var_os("LOCALAPPDATA").map(PathBuf::from).map(|p| p.join("omp/omp.exe")).unwrap_or_else(|| PathBuf::from("omp.exe")) }
    #[cfg(unix)]
    { home().map(|path| path.join(".local/bin/omp")).unwrap_or_else(|_| PathBuf::from("omp")) }
}
fn digest(bytes: &[u8]) -> String { let mut h = Sha256::new(); h.update(bytes); format!("sha256:{:x}", h.finalize()) }

/// 터미널에서만 진행을 보여 준다. 앱이 출력을 받으면(파이프) 오류 문장에 진행 줄이 섞이지 않게 한다.
pub(crate) fn cli_progress(message: &str) {
    use std::io::IsTerminal;
    if std::io::stderr().is_terminal() {
        let _ = writeln!(std::io::stderr(), "{message}");
    }
}


fn managed_block(text: &str) -> Result<Option<(usize, usize)>, ApiError> {
    let starts: Vec<_> = text.match_indices(CONFIG_BEGIN).map(|(i, _)| i).collect();
    let ends: Vec<_> = text.match_indices(CONFIG_END).map(|(i, _)| i).collect();
    if starts.len() > 1 || ends.len() > 1 || starts.len() != ends.len() || starts.first().zip(ends.first()).is_some_and(|(s, e)| s > e) {
        return Err(ApiError::new("CONFIG_CONFLICT", "omp 설정의 Ojak 관리 구간이 겹치거나 손상돼 자동으로 바꾸지 않아요."));
    }
    Ok(starts.first().zip(ends.first()).map(|(start, end)| (*start, *end + CONFIG_END.len())))
}

fn has_top_level_auth(text: &str) -> bool {
    text.lines().any(|line| line.trim_end() == "auth:")
}

/// OMP가 설정을 재작성하면 주석 마커가 사라지고 값만 남는다.
/// 마커가 없어도 우리가 적용한 broker 설정이면 연결로 인정하고 범위를 찾는다.
fn applied_block(text: &str) -> Option<(usize, usize)> {
    let target = format!("auth:\n  broker:\n    url: {BROKER_URL}");
    let start = text.find(&target)?;
    let mut end = start + target.len();
    if text[end..].starts_with('\n') { end += 1; }
    Some((start, end))
}

/// 연결 판정에 쓰는 설정 범위. 관리 블록이 우선이고, 없으면 적용된 값을 본다.
fn connected_block(text: &str) -> Result<Option<(usize, usize)>, ApiError> {
    Ok(managed_block(text)?.or_else(|| applied_block(text)))
}
fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), ApiError> {
    let parent = path.parent().ok_or_else(|| ApiError::new("INVALID_PATH", "omp 설정 경로가 올바르지 않아요."))?;
    fs::create_dir_all(parent).map_err(|_| ApiError::new("CONFIG_WRITE_FAILED", "omp 설정 폴더를 만들지 못했어요."))?;
    let existing = fs::symlink_metadata(path);
    if existing.as_ref().is_ok_and(|meta| meta.file_type().is_symlink()) {
        return Err(ApiError::new("CONFIG_CONFLICT", "omp 설정 파일이 바로가기라 자동으로 바꾸지 않아요."));
    }
    #[cfg(unix)]
    let mode = existing.as_ref().ok().filter(|meta| meta.is_file()).map(|meta| meta.permissions().mode() & 0o777).unwrap_or(0o600);
    let tmp = parent.join(format!(".aam-write-{}.tmp", aam_protocol::new_id()));
    let result = (|| {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        let mut file = aam_protocol::secure::private_options(&mut options).open(&tmp)
            .map_err(|_| ApiError::new("CONFIG_WRITE_FAILED", "omp 설정 임시 파일을 쓰지 못했어요."))?;
        file.write_all(bytes).map_err(|_| ApiError::new("CONFIG_WRITE_FAILED", "omp 설정 임시 파일을 쓰지 못했어요."))?;
        #[cfg(unix)]
        file.set_permissions(fs::Permissions::from_mode(mode)).map_err(|_| ApiError::new("CONFIG_WRITE_FAILED", "omp 설정 권한을 그대로 두지 못했어요."))?;
        #[cfg(windows)]
        aam_protocol::secure::restrict_file(&tmp).map_err(|_| ApiError::new("CONFIG_WRITE_FAILED", "omp 설정 권한을 보호하지 못했어요."))?;
        file.sync_all().map_err(|_| ApiError::new("CONFIG_WRITE_FAILED", "omp 설정 임시 파일을 쓰지 못했어요."))?;
        fs::rename(&tmp, path).map_err(|_| ApiError::new("CONFIG_WRITE_FAILED", "omp 설정을 안전하게 바꾸지 못했어요."))
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

fn read_config() -> Result<(PathBuf, String), ApiError> {
    let path = config_path()?;
    if fs::symlink_metadata(yaml_path()?).is_ok() { return Err(ApiError::new("CONFIG_CONFLICT", "omp config.yml과 config.yaml이 같이 있어 자동으로 바꾸지 않아요.")); }
    let text = match fs::read_to_string(&path) {
        Ok(value) => value,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(_) => return Err(ApiError::new("CONFIG_READ_FAILED", "omp 설정을 읽지 못했어요. 기존 파일은 그대로 둬요.")),
    };
    managed_block(&text)?;
    Ok((path, text))
}

fn load_receipt(paths: &Paths) -> Option<Receipt> { serde_json::from_slice(&fs::read(receipt_path(paths)).ok()?).ok() }

fn save_receipt(paths: &Paths, receipt: &Receipt) -> Result<(), ApiError> {
    fs::create_dir_all(&paths.home).map_err(|_| ApiError::new("BROKER_STATE_FAILED", "로그인 연결 기록 폴더를 만들지 못했어요."))?;
    let path = receipt_path(paths);
    if fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink()) { return Err(ApiError::new("BROKER_STATE_CONFLICT", "로그인 연결 기록이 바로가기라 바꾸지 않아요.")); }
    let bytes = serde_json::to_vec(receipt).map_err(|_| ApiError::new("BROKER_STATE_FAILED", "로그인 연결 기록을 만들지 못했어요."))?;
    fs::write(&path, bytes).map_err(|_| ApiError::new("BROKER_STATE_FAILED", "로그인 연결 기록을 쓰지 못했어요."))?;
    // 승격 실행에서도 소유자가 현재 사용자로 남도록 명시한다(Windows의 기본 소유자는 Administrators일 수 있다).
    aam_protocol::secure::restrict_file(&path).map_err(|_| ApiError::new("BROKER_STATE_FAILED", "로그인 연결 기록을 보호하지 못했어요."))
}

/// 인증된 broker 요청. 성공 응답 본문만 돌려주며 비밀을 저장하거나 오류에 담지 않는다.
fn broker_request(method: &str, path: &str, body: Option<&str>) -> Result<Vec<u8>, ApiError> {
    let token = fs::read_to_string(token_path()?).map_err(|_| ApiError::new("BROKER_NOT_READY", "omp 로그인 연결 토큰을 읽지 못했어요."))?;
    let mut stream = TcpStream::connect(("127.0.0.1", BROKER_PORT)).map_err(|_| ApiError::new("BROKER_UNAVAILABLE", "omp 로그인 연결에 닿지 못했어요."))?;
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
    stream.set_write_timeout(Some(Duration::from_secs(5))).ok();
    let payload = body.map(|body| format!("Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}", body.len())).unwrap_or_else(|| "\r\n".to_owned());
    let request = format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {}\r\nConnection: close\r\n{payload}", token.trim());
    stream.write_all(request.as_bytes()).map_err(|_| ApiError::new("BROKER_UNAVAILABLE", "OMP broker 인증 요청을 보내지 못했습니다."))?;
    let mut response = Vec::new();
    stream.read_to_end(&mut response).map_err(|_| ApiError::new("BROKER_UNAVAILABLE", "OMP broker 인증 응답을 읽지 못했습니다."))?;
    // 전송 계층은 바이트로 다룬다. 멀티바이트 문자가 chunk 경계에서 갈려도 손상되지 않는다.
    let split = response.windows(4).position(|window| window == b"\r\n\r\n").ok_or_else(|| ApiError::new("BROKER_PROTOCOL_MISMATCH", "OMP broker 응답 형식을 해석하지 못했습니다."))?;
    let head = String::from_utf8_lossy(&response[..split]).into_owned();
    let body = &response[split + 4..];
    if !head.starts_with("HTTP/1.1 200") && !head.starts_with("HTTP/1.0 200") { return Err(ApiError::new("BROKER_AUTH_FAILED", "OMP broker 인증 요청에 실패했습니다.")); }
    let chunked = head.lines().any(|line| {
        let (name, value) = line.split_once(':').unwrap_or(("", ""));
        name.trim().eq_ignore_ascii_case("transfer-encoding") && value.to_ascii_lowercase().contains("chunked")
    });
    decode_body(body, chunked)
}

/// 인증된 snapshot 조회. 응답 본문의 비밀은 저장하거나 반환하지 않는다.
fn authenticated_snapshot() -> Result<Snapshot, ApiError> {
    let decoded = broker_request("GET", "/v1/snapshot", None)?;
    let text = std::str::from_utf8(&decoded).map_err(|_| ApiError::new("BROKER_PROTOCOL_MISMATCH", "OMP broker 응답이 UTF-8이 아닙니다."))?;
    parse_snapshot(text)
}

/// broker에 로그인 정보가 있는 공급자 ID 목록.
pub(crate) fn logged_in_providers() -> Result<Vec<String>, ApiError> {
    authenticated_snapshot().map(|snapshot| snapshot.providers)
}


/// HTTP chunked 전송을 바이트 단위로 해제한다. 결합이 끝난 뒤에만 UTF-8로 해석한다.
fn decode_body(body: &[u8], chunked: bool) -> Result<Vec<u8>, ApiError> {
    if !chunked { return Ok(body.to_vec()); }
    let truncated = || ApiError::new("BROKER_PROTOCOL_MISMATCH", "OMP broker chunked 응답이 잘렸습니다.");
    let mut rest = body;
    let mut decoded = Vec::with_capacity(body.len());
    loop {
        let line_end = rest.windows(2).position(|window| window == b"\r\n").ok_or_else(truncated)?;
        let header = std::str::from_utf8(&rest[..line_end]).map_err(|_| ApiError::new("BROKER_PROTOCOL_MISMATCH", "OMP broker chunk 헤더를 해석하지 못했습니다."))?;
        let size = usize::from_str_radix(header.split(';').next().unwrap_or("").trim(), 16).map_err(|_| ApiError::new("BROKER_PROTOCOL_MISMATCH", "OMP broker chunk 크기를 해석하지 못했습니다."))?;
        let tail = &rest[line_end + 2..];
        if size == 0 { return Ok(decoded); }
        if tail.len() < size + 2 { return Err(truncated()); }
        decoded.extend_from_slice(&tail[..size]);
        if &tail[size..size + 2] != b"\r\n" { return Err(ApiError::new("BROKER_PROTOCOL_MISMATCH", "OMP broker chunk 구분자가 올바르지 않습니다.")); }
        rest = &tail[size + 2..];
    }
}

/// snapshot 본문에서 계정 수와 provider 목록만 추출한다. 같은 provider의 여러 계정을 하나로 합치지 않는다.
fn parse_snapshot(body: &str) -> Result<Snapshot, ApiError> {
    let json = body.trim();
    let parsed: serde_json::Value = serde_json::Deserializer::from_str(json)
        .into_iter::<serde_json::Value>()
        .next()
        .and_then(Result::ok)
        .ok_or_else(|| ApiError::new("BROKER_PROTOCOL_MISMATCH", "OMP broker snapshot을 해석하지 못했습니다."))?;
    let credentials = parsed.get("credentials").and_then(|value| value.as_array()).ok_or_else(|| ApiError::new("BROKER_PROTOCOL_MISMATCH", "OMP broker snapshot 응답이 올바르지 않습니다."))?;
    let accounts = credentials.iter().filter(|entry| entry.get("provider").and_then(|value| value.as_str()).is_some_and(|value| !value.is_empty())).count();
    let mut providers: Vec<String> = credentials
        .iter()
        .filter_map(|entry| entry.get("provider").and_then(|value| value.as_str()).filter(|value| !value.is_empty()).map(str::to_owned))
        .collect();
    providers.sort();
    providers.dedup();
    Ok(Snapshot { accounts, providers })
}

pub fn status(paths: &Paths) -> Result<Status, ApiError> {
    let (config, text) = read_config()?;
    let block = connected_block(&text)?;
    let token = token_path()?;
    let snapshot = if block.is_some() { authenticated_snapshot().ok() } else { None };
    #[cfg(unix)]
    let supervised = { let _ = paths; launchctl(&["print".into(), agent_target()]).unwrap_or(false) };
    #[cfg(windows)]
    let supervised = windows::supervised(paths);
    Ok(Status {
        supported: true,
        connected: snapshot.is_some(),
        config_path: config.display().to_string(),
        token_path: token.display().to_string(),
        url: BROKER_URL.into(),
        managed_block: block.is_some(),
        source_digest: digest(text.as_bytes()),
        supervised,
        account_count: snapshot.as_ref().map(|value| value.accounts),
        providers: snapshot.map(|value| value.providers).unwrap_or_default(),
    })
}

/// 재부팅·종료 후에도 기존 OMP 실행이 끊기지 않도록 LaunchAgent로 broker를 감독한다.
#[cfg(unix)]
fn agent_plist_path() -> Result<PathBuf, ApiError> {
    Ok(home()?.join(format!("Library/LaunchAgents/{AGENT_LABEL}.plist")))
}

#[cfg(unix)]
fn xml_escape(value: &str) -> String {
    value.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

#[cfg(unix)]
fn launchctl(args: &[String]) -> Result<bool, ApiError> {
    let status = Command::new("/bin/launchctl")
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|_| ApiError::new("BROKER_AGENT_FAILED", "launchctl을 실행하지 못했습니다."))?;
    Ok(status.success())
}

#[cfg(unix)]
fn agent_target() -> String { format!("gui/{}/{AGENT_LABEL}", unsafe { libc::geteuid() }) }

fn require_executable(path: &Path) -> Result<PathBuf, ApiError> {
    if !path.is_absolute() {
        return Err(ApiError::new("BROKER_RUNTIME_UNVERIFIED", "원본 OMP 실행 파일의 절대 경로를 확인하지 못했습니다."));
    }
    let meta = fs::metadata(path).map_err(|_| ApiError::new("BROKER_RUNTIME_UNVERIFIED", "원본 OMP 실행 파일을 확인하지 못했습니다."))?;
    #[cfg(unix)]
    let executable = meta.permissions().mode() & 0o111 != 0;
    #[cfg(windows)]
    let executable = path.extension().and_then(|extension| extension.to_str()).is_some_and(|extension| extension.eq_ignore_ascii_case("exe"));
    if !meta.is_file() || !executable {
        return Err(ApiError::new("BROKER_RUNTIME_UNVERIFIED", "원본 OMP 실행 파일이 실행 가능하지 않습니다."));
    }
    Ok(path.to_path_buf())
}

#[cfg(unix)]
fn agent_plist(paths: &Paths) -> Result<String, ApiError> {
    let omp = require_executable(&std::env::var_os("OMP_NATIVE_BIN").map(PathBuf::from).unwrap_or_else(native_omp))?;
    Ok(format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\"><dict>\n<key>Label</key><string>{AGENT_LABEL}</string>\n<key>ProgramArguments</key><array><string>{}</string><string>auth-broker</string><string>serve</string></array>\n<key>RunAtLoad</key><true/><key>KeepAlive</key><true/>\n<key>ProcessType</key><string>Background</string>\n<key>EnvironmentVariables</key><dict><key>HOME</key><string>{}</string><key>AAM_HOME</key><string>{}</string></dict>\n</dict></plist>\n",
        xml_escape(&omp.to_string_lossy()),
        xml_escape(&home()?.to_string_lossy()),
        xml_escape(&paths.home.to_string_lossy()),
    ))
}

#[cfg(unix)]
fn agent_receipt_path(paths: &Paths) -> PathBuf { paths.home.join("omp-broker-agent.json") }

#[cfg(unix)]
fn load_agent_receipt(paths: &Paths) -> Option<AgentReceipt> { serde_json::from_slice(&fs::read(agent_receipt_path(paths)).ok()?).ok() }

/// 기존 로그인을 유지한 채 broker를 준비한다. 계정 복사·재로그인은 하지 않는다.
/// 앱이 만들지 않은 LaunchAgent는 내리거나 지우지 않는다.
#[cfg(unix)]
fn ensure_broker(paths: &Paths) -> Result<(), ApiError> {
    let plist_path = agent_plist_path()?;
    let contents = agent_plist(paths)?;
    if fs::symlink_metadata(&plist_path).is_ok_and(|meta| meta.file_type().is_symlink()) {
        return Err(ApiError::new("BROKER_AGENT_CONFLICT", "broker LaunchAgent 파일이 symlink라 변경하지 않습니다."));
    }
    let installed = fs::read_to_string(&plist_path).ok();
    // 소유 기록이 있거나, 내용이 앱이 생성하는 것과 정확히 같으면 앱 소유로 본다.
    let owned = installed.as_deref() == Some(contents.as_str())
        || load_agent_receipt(paths).is_some_and(|receipt| Some(receipt.plist_digest.as_str()) == installed.as_deref().map(|text| digest(text.as_bytes())).as_deref());
    if installed.is_some() && !owned {
        return Err(ApiError::new("BROKER_AGENT_CONFLICT", "앱 소유가 아닌 broker LaunchAgent가 이미 있어 변경하지 않습니다. 해당 LaunchAgent를 직접 정리한 뒤 다시 시도해 주세요."));
    }
    let created = installed.is_none();
    if installed.as_deref() != Some(contents.as_str()) || load_agent_receipt(paths).is_none() {
        if let Some(parent) = plist_path.parent() {
            fs::create_dir_all(parent).map_err(|_| ApiError::new("BROKER_AGENT_FAILED", "LaunchAgents 폴더를 만들지 못했습니다."))?;
        }
        if installed.is_some() {
            let _ = launchctl(&["bootout".into(), agent_target()]);
            // bootout은 비동기다. 등록이 사라질 때까지 기다려야 bootstrap이 성공한다.
            for _ in 0..50 {
                if !launchctl(&["print".into(), agent_target()]).unwrap_or(false) { break; }
                std::thread::sleep(Duration::from_millis(100));
            }
        }
        atomic_write(&plist_path, contents.as_bytes())?;
        fs::create_dir_all(&paths.home).map_err(|_| ApiError::new("BROKER_STATE_FAILED", "broker 상태 폴더를 만들지 못했습니다."))?;
        let receipt = AgentReceipt { plist_digest: digest(contents.as_bytes()) };
        let bytes = serde_json::to_vec(&receipt).map_err(|_| ApiError::new("BROKER_STATE_FAILED", "broker LaunchAgent 소유 기록을 만들지 못했습니다."))?;
        fs::write(agent_receipt_path(paths), bytes).map_err(|_| ApiError::new("BROKER_STATE_FAILED", "broker LaunchAgent 소유 기록을 쓰지 못했습니다."))?;
    }
    if !launchctl(&["print".into(), agent_target()])? {
        let mut bootstrapped = false;
        for _ in 0..30 {
            if launchctl(&["bootstrap".into(), format!("gui/{}", unsafe { libc::geteuid() }), plist_path.to_string_lossy().into_owned()])?
                || launchctl(&["print".into(), agent_target()])?
            {
                bootstrapped = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        if !bootstrapped {
            return Err(ApiError::new("BROKER_AGENT_FAILED", "broker LaunchAgent 등록에 실패했습니다. 기존 설정은 그대로 두었습니다."));
        }
    }
    let started = Instant::now();
    let mut announced = false;
    let mut noted = 0u64;
    for _ in 0..100 {
        if authenticated_snapshot().is_ok() { return Ok(()); }
        if !announced {
            cli_progress("omp broker를 기다리는 중…");
            announced = true;
        }
        let elapsed = started.elapsed().as_secs();
        if elapsed >= 5 && elapsed / 5 > noted {
            noted = elapsed / 5;
            cli_progress(&format!("omp broker를 기다리는 중… {elapsed}초"));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    // 이번 호출이 새로 만든 agent만 정리한다. 기존 agent는 보존한다.
    if created {
        let _ = launchctl(&["bootout".into(), agent_target()]);
        let _ = fs::remove_file(&plist_path);
        let _ = fs::remove_file(agent_receipt_path(paths));
    }
    Err(ApiError::new("BROKER_NOT_READY", "OMP auth broker가 인증 조회 가능 상태가 되지 못했습니다."))
}

#[cfg(windows)]
fn ensure_broker(paths: &Paths) -> Result<(), ApiError> {
    windows::ensure(paths)
}

pub fn connect(paths: &Paths) -> Result<Status, ApiError> {
    let (path, current) = read_config()?;
    let existing = connected_block(&current)?;
    if has_top_level_auth(&current) && existing.is_none() { return Err(ApiError::new("CONFIG_CONFLICT", "기존 OMP auth 설정이 있어 자동으로 중첩하지 않습니다.")); }
    let owned = load_receipt(paths).is_some();
    if existing.is_some() && !owned {
        return Err(ApiError::new("CONFIG_CONFLICT", "AAM 연결 소유권 기록이 없어 기존 설정을 교체하지 않습니다."));
    }
    // OMP가 설정을 재작성해 마커가 사라졌어도 값이 우리 것이면 그대로 인정한다. 다시 쓰지 않는다.
    if managed_block(&current)?.is_none() && applied_block(&current).is_some() && owned {
        ensure_broker(paths)?;
        save_receipt(paths, &Receipt { applied_digest: digest(current.as_bytes()) })?;
        return status(paths);
    }
    if let Some(receipt) = load_receipt(paths) {
        if existing.is_some() && receipt.applied_digest != digest(current.as_bytes()) {
            return Err(ApiError::new("CONFIG_CONFLICT", "AAM 관리 설정이 연결 후 변경되어 자동 교체하지 않습니다."));
        }
    }
    ensure_broker(paths)?;
    let managed = format!("{CONFIG_BEGIN}\nauth:\n  broker:\n    url: {BROKER_URL}\n{CONFIG_END}");
    let next = match existing {
        Some((start, end)) => format!("{}{}{}", &current[..start], managed, &current[end..]),
        None if current.is_empty() => format!("{managed}\n"),
        None => format!("{}\n\n{managed}\n", current),
    };
    atomic_write(&path, next.as_bytes())?;
    save_receipt(paths, &Receipt { applied_digest: digest(next.as_bytes()) })?;
    status(paths)
}

pub fn disconnect(paths: &Paths) -> Result<Status, ApiError> {
    let (path, current) = read_config()?;
    let Some((start, end)) = connected_block(&current)? else { return status(paths); };
    let receipt = load_receipt(paths).ok_or_else(|| ApiError::new("CONFIG_CONFLICT", "AAM 연결 소유권 기록이 없어 설정을 변경하지 않습니다."))?;
    // 마커가 남아 있으면 적용 당시와 동일해야 한다. 마커가 사라진 경우는 우리가 적용한 값 자체를 근거로 삼는다.
    if managed_block(&current)?.is_some() && receipt.applied_digest != digest(current.as_bytes()) {
        return Err(ApiError::new("CONFIG_CONFLICT", "OMP 설정이 연결 후 변경되어 자동 제거하지 않습니다."));
    }
    let mut next = String::with_capacity(current.len());
    next.push_str(&current[..start]);
    next.push_str(&current[end..]);
    atomic_write(&path, next.as_bytes())?;
    let _ = fs::remove_file(receipt_path(paths));
    status(paths)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test] fn duplicate_markers_are_rejected() { let text = format!("{CONFIG_BEGIN}{CONFIG_END}{CONFIG_BEGIN}{CONFIG_END}"); assert!(managed_block(text.as_str()).is_err()); }
    #[test] fn malformed_markers_are_rejected() { assert!(managed_block(CONFIG_BEGIN).is_err()); }
    #[test] fn same_provider_accounts_are_counted_separately() {
        let body = r#"{"credentials":[{"id":1,"provider":"anthropic"},{"id":2,"provider":"anthropic"},{"id":3,"provider":"zai"}]}"#;
        let snapshot = parse_snapshot(body).unwrap();
        assert_eq!(snapshot.accounts, 3);
        assert_eq!(snapshot.providers, vec!["anthropic".to_string(), "zai".to_string()]);
    }
    #[test] fn json_split_across_chunks_is_decoded() {
        let body = b"14\r\n{\x22credentials\x22:[{\x22pr\r\n3d\r\novider\x22:\x22anthropic\x22,\x22id\x22:1},{\x22provider\x22:\x22anthropic\x22,\x22id\x22:2}]}\r\n0\r\n\r\n";
        let decoded = decode_body(body, true).unwrap();
        assert_eq!(decoded, b"{\x22credentials\x22:[{\x22provider\x22:\x22anthropic\x22,\x22id\x22:1},{\x22provider\x22:\x22anthropic\x22,\x22id\x22:2}]}");
        assert_eq!(parse_snapshot(std::str::from_utf8(&decoded).unwrap()).unwrap().accounts, 2);
    }
    #[test] fn chunk_split_inside_multibyte_character_is_decoded() {
        let body = b"39\r\n{\x22credentials\x22:[{\x22provider\x22:\x22anthropic\x22,\x22id\x22:1,\x22email\x22:\x22\xed\r\nf\r\n\x95\x9c\xea\xb8\x80\xec\x9d\xb4\xeb\xa6\x84\x22}]}\r\n0\r\n\r\n";
        let decoded = decode_body(body, true).unwrap();
        assert_eq!(decoded, b"{\x22credentials\x22:[{\x22provider\x22:\x22anthropic\x22,\x22id\x22:1,\x22email\x22:\x22\xed\x95\x9c\xea\xb8\x80\xec\x9d\xb4\xeb\xa6\x84\x22}]}");
        let snapshot = parse_snapshot(std::str::from_utf8(&decoded).unwrap()).unwrap();
        assert_eq!(snapshot.accounts, 1);
    }
    #[test] fn empty_snapshot_reports_no_accounts() {
        let snapshot = parse_snapshot(r#"{"credentials":[]}"#).unwrap();
        assert_eq!(snapshot.accounts, 0);
        assert!(snapshot.providers.is_empty());
    }
    #[test] fn rewritten_config_is_recognized() {
        // OMP가 설정을 재작성하면 주석 마커가 사라진다. 값이 남아 있으면 연결로 인정해야 한다.
        let text = format!("theme: dark\nauth:\n  broker:\n    url: {BROKER_URL}\n");
        let (start, end) = connected_block(&text).unwrap().unwrap();
        assert_eq!(&text[start..end], format!("auth:\n  broker:\n    url: {BROKER_URL}\n"));
        assert!(managed_block(&text).unwrap().is_none());
    }
    #[test] fn foreign_broker_url_is_not_recognized() {
        let text = "auth:\n  broker:\n    url: http://127.0.0.1:9999\n";
        assert!(connected_block(text).unwrap().is_none());
    }
    #[test] fn nested_auth_key_is_not_top_level() { assert!(!has_top_level_auth("providers:\n  auth:\n")); assert!(has_top_level_auth("auth:\n  broker:\n")); }
    #[cfg(unix)]
    #[test]
    fn config_write_preserves_mode_and_refuses_symlink() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("aam-broker-{}", aam_protocol::new_id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.yml");
        fs::write(&path, b"old").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
        atomic_write(&path, b"new").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"new");
        assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o640);
        let link = dir.join("link.yml");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(atomic_write(&link, b"nope").is_err());
        assert_eq!(fs::read(&path).unwrap(), b"new");
        let created = dir.join("fresh.yml");
        atomic_write(&created, b"fresh").unwrap();
        assert_eq!(fs::metadata(&created).unwrap().permissions().mode() & 0o777, 0o600);
        fs::remove_dir_all(&dir).unwrap();
    }
    #[cfg(unix)]
    #[test]
    fn omp_binary_must_be_absolute_and_executable() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("aam-omp-{}", aam_protocol::new_id()));
        fs::create_dir_all(&dir).unwrap();
        let bin = dir.join("omp");
        fs::write(&bin, b"#!/bin/sh\n").unwrap();
        fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(require_executable(&bin).unwrap(), bin);
        fs::set_permissions(&bin, fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(require_executable(&bin).unwrap_err().code, "BROKER_RUNTIME_UNVERIFIED");
        assert!(require_executable(Path::new("omp")).is_err());
        fs::remove_dir_all(&dir).unwrap();
    }

}
