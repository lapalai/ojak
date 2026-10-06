//! 외부에서 시작된 대화의 소유 계정을 실제 파일 근거로만 확인합니다.
//! 추정한 계정으로 대화를 인계하지 않으며, 대화 본문은 읽지 않습니다.
use aam_protocol::{Account, ApiError};
use serde_json::Value;
use std::{
    io::{BufRead, Read},
    path::{Path, PathBuf},
};

const MAX_HEADER: u64 = 64 * 1024;
const MAX_PROJECT_DIRS: usize = 8192;

/// 확인된 소유 계정과 근거입니다. 근거 없이는 값을 만들지 않습니다.
#[derive(Debug, Clone)]
pub struct SessionOwner {
    pub account_id: String,
    /// 실제 위치로 정규화한 대화 식별자입니다. 이후 조회·저장은 이 값을 사용합니다.
    pub native_session_id: String,
    pub cwd: Option<String>,
    pub evidence: String,
}

fn unsupported(message: &str) -> ApiError {
    ApiError::new("TAKEOVER_UNSUPPORTED", message)
}
fn unknown(message: &str) -> ApiError {
    ApiError::new("TAKEOVER_OWNER_UNKNOWN", message)
}

fn open_regular(path: &Path) -> Result<std::fs::File, ApiError> {
    let file = aam_protocol::secure::open_read_no_follow(path)
        .map_err(|_| unknown("대화 파일을 안전하게 열 수 없습니다."))?;
    let metadata = file
        .metadata()
        .map_err(|_| unknown("대화 파일 정보를 확인할 수 없습니다."))?;
    if !metadata.is_file() || !aam_protocol::secure::file_owned_by_me(&file).unwrap_or(false) {
        return Err(unknown(
            "대화 파일의 소유권 또는 종류가 올바르지 않습니다.",
        ));
    }
    Ok(file)
}

fn profile(account: &Account) -> Option<PathBuf> {
    let path = PathBuf::from(account.profile_path.as_deref()?);
    path.canonicalize().ok().filter(|resolved| resolved.is_dir())
}

/// 계정 이름은 사람이 확인할 수 있도록 두고 프로필 전체 경로는 근거 문장에 넣지 않습니다.
fn label(account: &Account) -> String {
    match &account.email {
        Some(email) => format!("{} · {email}", account.label),
        None => account.label.clone(),
    }
}

/// Claude 대화는 계정 프로필 안의 실제 대화 파일 위치로만 소유 계정을 확인합니다.
pub fn claude_session_owner(accounts: &[Account], native: &str) -> Result<SessionOwner, ApiError> {
    let uuid = uuid::Uuid::parse_str(native)
        .map_err(|_| unsupported("Claude 대화 식별자는 완전한 UUID여야 합니다."))?
        .to_string();
    let mut found: Vec<(&Account, Option<String>)> = Vec::new();
    let mut inspected = 0;
    for account in accounts.iter().filter(|account| account.tool == "claude") {
        let Some(profile) = profile(account) else {
            continue;
        };
        let Ok(projects) = std::fs::read_dir(profile.join("projects")) else {
            continue;
        };
        for entry in projects.flatten() {
            inspected += 1;
            if inspected > MAX_PROJECT_DIRS {
                return Err(unknown(
                    "프로필의 프로젝트 폴더가 조회 한도를 넘어 소유 계정을 확정하지 못했습니다.",
                ));
            }
            if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                continue;
            }
            let file = entry.path().join(format!("{uuid}.jsonl"));
            if !std::fs::symlink_metadata(&file).is_ok_and(|data| data.is_file()) {
                continue;
            }
            found.push((account, claude_cwd(&file)));
            break;
        }
    }
    match found.as_slice() {
        [(account, cwd)] => Ok(SessionOwner {
            account_id: account.id.clone(),
            native_session_id: uuid,
            cwd: cwd.clone(),
            evidence: format!(
                "'{}' 계정 프로필의 projects 폴더에서 같은 대화 파일을 확인했습니다.",
                label(account)
            ),
        }),
        [] => Err(unknown(
            "연결된 Claude 계정 프로필에서 이 대화 파일을 찾지 못했습니다. 계정을 추정하지 않습니다.",
        )),
        _ => Err(unknown(
            "여러 계정 프로필에 같은 대화 파일이 있어 소유 계정을 하나로 확정하지 않습니다.",
        )),
    }
}

/// Claude는 각 기록 행에 작업 폴더를 남깁니다. 없으면 비워 두고 추정하지 않습니다.
fn claude_cwd(path: &Path) -> Option<String> {
    let file = open_regular(path).ok()?;
    let mut reader = std::io::BufReader::new(file.take(MAX_HEADER));
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    let value: Value = serde_json::from_str(&line).ok()?;
    let cwd = value.get("cwd")?.as_str()?;
    Path::new(cwd)
        .canonicalize()
        .ok()
        .filter(|resolved| resolved.is_dir())
        .map(|resolved| resolved.to_string_lossy().into_owned())
}

/// 공식 CLI 설정에 저장된 기본 모델을 읽습니다. 설정이 없으면 추측하지 않고 비웁니다.
/// 실행 시점의 셸 환경 변수와 CLI 인수는 이 경로로 확인할 수 없습니다.
pub fn configured_model(tool: &str, cwd: &Path, profile: Option<&str>) -> Option<(String, String)> {
    if tool != "claude" {
        return None;
    }
    let project = cwd.join(".claude");
    let sources = [
        (project.join("settings.local.json"), "프로젝트 개인 설정"),
        (project.join("settings.json"), "프로젝트 공유 설정"),
        (
            Path::new(profile.unwrap_or("")).join("settings.json"),
            "계정 프로필 설정",
        ),
    ];
    for (path, source) in sources {
        let Ok(file) = open_regular(&path) else {
            continue;
        };
        let mut text = String::new();
        if file.take(256 * 1024).read_to_string(&mut text).is_err() {
            continue;
        }
        let Ok(settings) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        // 앞선 설정 파일에 모델 값이 없으면 다음 우선순위 설정을 확인합니다.
        let Some(model) = settings.get("model").and_then(Value::as_str) else {
            continue;
        };
        if model.is_empty() || model.len() > 160 || model.chars().any(char::is_control) {
            return None;
        }
        return Some((model.to_owned(), source.into()));
    }
    None
}

/// 도구별로 확인 가능한 대화 형태만 인계 대상으로 받습니다.
pub fn session_owner(
    accounts: &[Account],
    tool: &str,
    native: &str,
) -> Result<SessionOwner, ApiError> {
    match tool {
        "claude" => claude_session_owner(accounts, native),
        _ => Err(unsupported(
            "이 도구는 검증된 대화 인계를 아직 지원하지 않습니다.",
        )),
    }
}
