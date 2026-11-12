//! `aam omp-bridge update-omp`: 18.12 omp가 새 업데이터(`omp update`)를 갖기 전에 Ojak 쪽에서 omp를 올려 주는 경로.
//!
//! 배포된 18.12 업데이터는 Ojak 서비스 버전을 확인하지 않고 `omp-compatible.json`(schema 1)과 접미사 없는 asset 이름만 읽는다.
//! 그래서 릴리스는 그 이름을 더는 싣지 않고, 이 명령과 새 omp 업데이터가 같은 `omp-compatible-v2.json`(schema 2)과
//! `omp-v2-*` asset을 읽는다. 두 경로 모두 **실행 중인 서비스 버전이 manifest의 `minimumOjakService` 이상일 때만** 설치한다.
//!
//! **omp 전용 채널.** 앱 업데이터는 `/releases/latest`(`latest.json`)를 쓰므로 omp 릴리스는 절대 `latest`가 아니다.
//! 그래서 `/releases?per_page=30` 한 페이지(크기 제한)에서 태그가 `omp-vX.Y.Z-ojak.N`이고 공개된 non-draft·non-prerelease이며
//! v2 manifest 버전이 태그 버전과 같은 릴리스 중 가장 높은 버전을 고른다. 앱 릴리스는 이 목록에서 태그로 걸러진다.
//!
//! 안전 규칙:
//! - 공개 릴리스(draft·prerelease 아님)와 같은 릴리스의 asset만 쓴다. URL은 릴리스 다운로드 접두사와 정확히 같아야 하고,
//!   리다이렉트는 GitHub release-assets 호스트(https)만 따라간다. 다른 출처로 우회하지 않는다.
//! - 크기와 SHA-256이 manifest·릴리스 메타데이터와 모두 맞아야 한다. 서비스 버전을 알 수 없거나 낮으면 아무것도 받지 않고 멈춘다.
//! - 교체 전 백업을 만들고, 교체한 실행 파일이 `--version`으로 manifest 버전을 보고하지 않으면 백업으로 되돌린다.
//! - Windows에서 omp 프로세스가 파일을 쥐고 있으면 닫지 않고 거절한다.

use aam_protocol::ApiError;
use reqwest::{blocking::Client, header, redirect::Policy};
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant},
};

const RELEASE_API: &str = "https://api.github.com/repos/lapalai/ojak/releases?per_page=30";
const UPSTREAM_RELEASE_API: &str = "https://api.github.com/repos/can1357/oh-my-pi/releases/latest";
const DOWNLOAD_PREFIX: &str = "https://github.com/lapalai/ojak/releases/download/";
const REDIRECT_HOST: &str = "release-assets.githubusercontent.com";
const MANIFEST: &str = "omp-compatible-v2.json";
const RELEASE_LIMIT: u64 = 4 * 1024 * 1024;
/// 목록을 버전 내림차순으로 훑을 때 manifest를 실제로 받아 보는 최대 릴리스 수.
const MAX_MANIFEST_CHECKS: usize = 5;
const TAG_PREFIX: &str = "omp-v";
const MANIFEST_LIMIT: u64 = 64 * 1024;
const BINARY_LIMIT: u64 = 1024 * 1024 * 1024;
const PLATFORMS: [(&str, &str); 2] = [("darwin-arm64", "omp-v2-darwin-arm64"), ("win32-x64", "omp-v2-windows-x64.exe")];

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
const HOST_PLATFORM: Option<&str> = Some("darwin-arm64");
#[cfg(all(windows, target_arch = "x86_64"))]
const HOST_PLATFORM: Option<&str> = Some("win32-x64");
#[cfg(not(any(all(target_os = "macos", target_arch = "aarch64"), all(windows, target_arch = "x86_64"))))]
const HOST_PLATFORM: Option<&str> = None;

fn error(code: &str, message: impl Into<String>) -> ApiError {
    ApiError::new(code, message)
}

/// 어디에서 릴리스를 읽는지. 운영 값은 `Source::github()`뿐이고, 테스트만 고정 서버 주소를 넣는다.
pub(crate) struct Source {
    pub api: String,
    /// `<prefix><tag>/<asset>`이 되는 다운로드 접두사(끝 `/` 포함).
    pub download_prefix: String,
    pub redirect_scheme: &'static str,
    pub redirect_host: String,
}

impl Source {
    pub(crate) fn github() -> Self {
        Self { api: RELEASE_API.into(), download_prefix: DOWNLOAD_PREFIX.into(), redirect_scheme: "https", redirect_host: REDIRECT_HOST.into() }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Plan {
    pub version: String,
    pub minimum_ojak_service: String,
    pub size: u64,
    pub sha256: String,
    pub url: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Outcome {
    /// 이미 같거나 더 새 버전이라 아무것도 하지 않았다.
    UpToDate,
    /// `--check`: 서비스 버전이 충분하고 설치할 수 있다. 파일은 바꾸지 않았다.
    WouldUpdate,
    Updated,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum UpstreamStatus {
    Current,
    CompatibilityPending,
    Unknown,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Report {
    pub outcome: Outcome,
    pub installed: Option<String>,
    pub available: String,
    /// `available`은 Ojak 호환 채널 버전이다. 원본 최신 여부는 이 필드들로 따로 보고한다.
    pub upstream_version: Option<String>,
    pub upstream_status: UpstreamStatus,
    pub upstream_notice: String,
    pub service_version: Option<String>,
    pub minimum_ojak_service: String,
    pub path: String,
}

fn record(value: &Value) -> Option<&serde_json::Map<String, Value>> {
    value.as_object()
}

fn plain_number(text: &str) -> Option<u64> {
    (!text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit()) && (text == "0" || !text.starts_with('0'))).then(|| text.parse().ok())?
}

/// `major.minor.patch`. `-rc.1`·`+build` 꼬리는 버린다(launcher `service_version`과 같은 규칙).
fn triple(text: &str) -> Option<(u64, u64, u64)> {
    let core = text.trim().split(['-', '+']).next()?;
    let mut parts = core.split('.');
    let triple = (plain_number(parts.next()?)?, plain_number(parts.next()?)?, plain_number(parts.next()?)?);
    parts.next().is_none().then_some(triple)
}

pub(crate) fn service_meets(actual: Option<&str>, minimum: &str) -> bool {
    matches!((actual.and_then(triple), triple(minimum)), (Some(have), Some(need)) if have >= need)
}

/// 포크 버전 `X.Y.Z-ojak.N` → ((X,Y,Z), N). 다른 모양은 `None`.
fn fork_version(text: &str) -> Option<((u64, u64, u64), u64)> {
    let (base, revision) = text.trim().split_once("-ojak.")?;
    if base.contains(['-', '+']) {
        return None;
    }
    let revision = plain_number(revision).filter(|n| *n >= 1)?;
    Some((triple(base)?, revision))
}

/// 목록의 한 항목이 omp 채널 후보인지. `Ok(None)`은 omp 릴리스가 아님(앱 릴리스 등), `Err`은 omp 태그인데 쓸 수 없는 릴리스.
fn candidate(release: &Value) -> Result<Option<(String, String)>, ApiError> {
    let invalid = |message: &str| error("OMP_UPDATE_RELEASE_INVALID", message.to_owned());
    let Some(release) = record(release) else { return Ok(None) };
    let tag = release.get("tag_name").and_then(Value::as_str).unwrap_or_default();
    if !tag.starts_with("omp-") {
        return Ok(None);
    }
    let version = tag.strip_prefix(TAG_PREFIX).filter(|version| fork_version(version).is_some() && *version == version.trim());
    let Some(version) = version else { return Err(invalid("omp 릴리스 태그 형식이 올바르지 않아요.")) };
    let published = release.get("published_at").and_then(Value::as_str).is_some_and(|text| !text.is_empty());
    if release.get("draft") != Some(&Value::Bool(false)) || release.get("prerelease") != Some(&Value::Bool(false)) || !published {
        return Err(invalid("공개된 omp 릴리스가 아니에요."));
    }
    Ok(Some((tag.to_owned(), version.to_owned())))
}

fn parse_release(release: &Value, tag: &str, source: &Source, platform: &str) -> Result<(Value, Plan), ApiError> {
    let invalid = |message: &str| error("OMP_UPDATE_RELEASE_INVALID", message.to_owned());
    let assets = release.get("assets").and_then(Value::as_array).ok_or_else(|| invalid("릴리스 asset 목록이 없어요."))?;
    let prefix = format!("{}{tag}/", source.download_prefix);
    let asset = |name: &str| -> Result<&serde_json::Map<String, Value>, ApiError> {
        let mut matches = assets.iter().filter_map(record).filter(|asset| asset.get("name").and_then(Value::as_str) == Some(name));
        let found = matches.next().filter(|_| matches.next().is_none());
        let found = found.filter(|asset| {
            asset.get("browser_download_url").and_then(Value::as_str) == Some(format!("{prefix}{name}").as_str())
                && asset.get("size").and_then(Value::as_u64).is_some_and(|size| size > 0 && size <= BINARY_LIMIT)
        });
        found.ok_or_else(|| error("OMP_UPDATE_RELEASE_INVALID", format!("같은 릴리스의 asset이 없거나 올바르지 않아요: {name}")))
    };
    let manifest_asset = asset(MANIFEST)?;
    let manifest_size = manifest_asset["size"].as_u64().unwrap_or(0);
    if manifest_size > MANIFEST_LIMIT {
        return Err(invalid("호환 manifest가 너무 커요."));
    }
    // 아직 내려받기 전이라 manifest는 호출자가 가져온다. 이 단계에서는 manifest 크기를 돌려주기 위해 Value로 감싼다.
    let wanted = PLATFORMS.iter().find(|(key, _)| *key == platform).map(|(_, name)| *name).ok_or_else(|| error("OMP_UPDATE_PLATFORM_UNSUPPORTED", "이 플랫폼용 호환 omp가 없어요."))?;
    let _ = asset(wanted)?; // manifest를 받기 전에 대상 asset이 릴리스에 있는지 먼저 확인한다.
    Ok((serde_json::json!({ "size": manifest_size, "assets": assets, "prefix": prefix }), Plan {
        version: String::new(),
        minimum_ojak_service: String::new(),
        size: 0,
        sha256: String::new(),
        url: format!("{prefix}{wanted}"),
    }))
}

fn parse_manifest(bytes: &[u8], tag_version: &str, release_assets: &Value, platform: &str, mut plan: Plan) -> Result<Plan, ApiError> {
    let invalid = |message: &str| error("OMP_UPDATE_MANIFEST_INVALID", message.to_owned());
    let manifest: Value = serde_json::from_slice(bytes).map_err(|_| invalid("호환 manifest를 읽지 못했어요."))?;
    let manifest = record(&manifest).ok_or_else(|| invalid("호환 manifest 형식이 아니에요."))?;
    let version = manifest.get("version").and_then(Value::as_str).unwrap_or_default();
    let minimum = manifest.get("minimumOjakService").and_then(Value::as_str).unwrap_or_default();
    let platforms = manifest.get("platforms").and_then(Value::as_object);
    if manifest.get("schemaVersion").and_then(Value::as_u64) != Some(2)
        || manifest.get("admissionProtocol").and_then(Value::as_u64) != Some(1)
        || fork_version(version).is_none_or(|(_, n)| n == 0)
        || version != version.trim()
        || triple(minimum).is_none()
        || minimum.contains(['-', '+'])
        || platforms.is_none()
    {
        return Err(invalid("호환 manifest의 스키마·버전·최소 서비스 버전이 올바르지 않아요."));
    }
    if version != tag_version {
        return Err(invalid("호환 manifest 버전이 릴리스 태그 버전과 달라요."));
    }
    let platforms = platforms.unwrap();
    let published = release_assets["assets"].as_array().cloned().unwrap_or_default();
    for (key, name) in PLATFORMS {
        let entry = platforms.get(key).and_then(Value::as_object).ok_or_else(|| invalid("호환 manifest에 플랫폼 항목이 없어요."))?;
        let size = entry.get("size").and_then(Value::as_u64).filter(|size| *size > 0 && *size <= BINARY_LIMIT);
        let digest = entry.get("sha256").and_then(Value::as_str).filter(|text| text.len() == 64 && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')));
        if entry.get("asset").and_then(Value::as_str) != Some(name) || size.is_none() || digest.is_none() {
            return Err(invalid("호환 manifest의 플랫폼 항목이 올바르지 않아요."));
        }
        let published = published.iter().filter_map(record).find(|asset| asset.get("name").and_then(Value::as_str) == Some(name)).ok_or_else(|| invalid("릴리스 asset과 manifest가 맞지 않아요."))?;
        let digest_matches = published.get("digest").is_none_or(|published| published.is_null() || published.as_str() == Some(format!("sha256:{}", digest.unwrap()).as_str()));
        if published.get("size").and_then(Value::as_u64) != size || !digest_matches {
            return Err(invalid("릴리스 asset과 manifest의 크기·해시가 맞지 않아요."));
        }
        if key == platform {
            plan.size = size.unwrap();
            plan.sha256 = digest.unwrap().to_owned();
        }
    }
    plan.version = version.to_owned();
    plan.minimum_ojak_service = minimum.to_owned();
    Ok(plan)
}

fn metadata_client() -> Result<Client, ApiError> {
    Client::builder()
        .redirect(Policy::none())
        .user_agent(concat!("aam-launcher/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|_| error("OMP_UPDATE_NETWORK", "내려받기 준비에 실패했어요."))
}

fn asset_client(source: &Source) -> Result<Client, ApiError> {
    let scheme = source.redirect_scheme;
    let host = source.redirect_host.clone();
    Client::builder()
        .redirect(Policy::custom(move |attempt| {
            let url = attempt.url();
            let allowed = attempt.previous().len() < 4
                && url.scheme() == scheme
                && url.host_str() == Some(host.as_str())
                && url.username().is_empty()
                && url.password().is_none()
                && url.fragment().is_none();
            if allowed { attempt.follow() } else { attempt.error("허용되지 않은 다운로드 리다이렉트예요.") }
        }))
        .user_agent(concat!("aam-launcher/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|_| error("OMP_UPDATE_NETWORK", "내려받기 준비에 실패했어요."))
}

fn network(_: impl std::fmt::Display) -> ApiError {
    error("OMP_UPDATE_NETWORK", "릴리스를 내려받지 못했어요. 기존 omp는 그대로 뒀어요.")
}

fn bounded(client: &Client, url: &str, limit: u64, expected: Option<u64>, timeout: Duration) -> Result<Vec<u8>, ApiError> {
    let response = client.get(url).header(header::ACCEPT, "application/json").timeout(timeout).send().map_err(network)?;
    if !response.status().is_success() {
        return Err(error("OMP_UPDATE_NETWORK", format!("릴리스 요청이 실패했어요 ({}). 기존 omp는 그대로 뒀어요.", response.status().as_u16())));
    }
    let mut body = Vec::new();
    response.take(limit + 1).read_to_end(&mut body).map_err(network)?;
    if body.len() as u64 > limit || expected.is_some_and(|size| size != body.len() as u64) {
        return Err(error("OMP_UPDATE_MANIFEST_INVALID", "릴리스 메타데이터 크기가 맞지 않아요."));
    }
    Ok(body)
}

/// omp 채널에서 가장 높은 유효 릴리스의 플랜을 만든다. 파일은 건드리지 않는다. `/releases/latest`는 읽지 않는다.
///
/// 항목 단위 오류(태그·상태·asset·manifest 불일치)는 그 릴리스만 건너뛰고 다음 후보로 간다. 전송 오류는 즉시 멈춘다.
pub(crate) fn plan(source: &Source, platform: &str) -> Result<Plan, ApiError> {
    let list: Value = serde_json::from_slice(&bounded(&metadata_client()?, &source.api, RELEASE_LIMIT, None, Duration::from_secs(30))?)
        .map_err(|_| error("OMP_UPDATE_RELEASE_INVALID", "Ojak 릴리스 목록을 확인하지 못했어요."))?;
    let list = list.as_array().ok_or_else(|| error("OMP_UPDATE_RELEASE_INVALID", "Ojak 릴리스 목록을 확인하지 못했어요."))?;
    let mut skipped = Vec::new();
    let mut candidates = Vec::new();
    for release in list {
        match candidate(release) {
            Ok(Some((tag, version))) => {
                let published = release.get("published_at").and_then(Value::as_str).unwrap_or_default().to_owned();
                candidates.push((fork_version(&version).expect("candidate versions are fork versions"), published, tag, version, release));
            }
            Ok(None) => {}
            Err(problem) => skipped.push(problem.message),
        }
    }
    candidates.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
    for (_, _, tag, version, release) in candidates.into_iter().take(MAX_MANIFEST_CHECKS) {
        let attempt = (|| {
            let (carried, plan) = parse_release(release, &tag, source, platform)?;
            let url = format!("{}{tag}/{MANIFEST}", source.download_prefix);
            let manifest = bounded(&asset_client(source)?, &url, MANIFEST_LIMIT, carried["size"].as_u64(), Duration::from_secs(30))?;
            parse_manifest(&manifest, &version, &carried, platform, plan)
        })();
        match attempt {
            Ok(plan) => return Ok(plan),
            Err(problem) if matches!(problem.code.as_str(), "OMP_UPDATE_RELEASE_INVALID" | "OMP_UPDATE_MANIFEST_INVALID") => skipped.push(format!("{tag}: {}", problem.message)),
            Err(problem) => return Err(problem),
        }
    }
    let detail = if skipped.is_empty() { String::new() } else { format!(" ({})", skipped.iter().take(3).cloned().collect::<Vec<_>>().join("; ")) };
    Err(error("OMP_UPDATE_RELEASE_INVALID", format!("Ojak omp 채널에서 공개된 유효 릴리스를 찾지 못했어요{detail}.")))
}

fn run_version(path: &Path) -> Option<String> {
    let mut child = Command::new(path).arg("--version").stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().ok()?;
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = (&mut stdout).take(4096).read_to_string(&mut text);
        text
    });
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let text = reader.join().ok()?;
                return status.success().then(|| text.trim().strip_prefix("omp/").map(str::to_owned)).flatten().filter(|version| !version.is_empty());
            }
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(25)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

fn native_executable(path: &Path) -> bool {
    let mut head = [0u8; 4];
    let Ok(mut file) = fs::File::open(path) else { return false };
    if file.read_exact(&mut head).is_err() {
        return false;
    }
    head == *b"\x7fELF"
        || head[..2] == *b"MZ"
        || matches!(u32::from_be_bytes(head), 0xfeedface | 0xfeedfacf | 0xcefaedfe | 0xcffaedfe | 0xcafebabe)
}

static ATTEMPT: AtomicU64 = AtomicU64::new(0);

fn sibling(target: &Path, suffix: &str) -> PathBuf {
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis());
    let mut name = target.file_name().map(|name| name.to_os_string()).unwrap_or_default();
    name.push(format!(".{stamp}.{}.{}.{suffix}", std::process::id(), ATTEMPT.fetch_add(1, Ordering::Relaxed)));
    target.with_file_name(name)
}

fn download(source: &Source, plan: &Plan, temp: &Path) -> Result<(), ApiError> {
    let fail = |message: &str| error("OMP_UPDATE_DOWNLOAD_FAILED", format!("{message} 기존 omp는 그대로 뒀어요."));
    let response = asset_client(source)?.get(&plan.url).timeout(Duration::from_secs(15 * 60)).send().map_err(network)?;
    if !response.status().is_success() {
        return Err(fail("omp를 내려받지 못했어요."));
    }
    let mut file = fs::OpenOptions::new().write(true).create_new(true).open(temp).map_err(|_| fail("임시 파일을 만들지 못했어요."))?;
    let mut hasher = Sha256::new();
    let mut written = 0u64;
    let mut buffer = vec![0u8; 256 * 1024];
    let mut response = response;
    loop {
        let read = response.read(&mut buffer).map_err(network)?;
        if read == 0 {
            break;
        }
        written += read as u64;
        if written > plan.size {
            return Err(fail("내려받은 크기가 manifest보다 커요."));
        }
        hasher.update(&buffer[..read]);
        file.write_all(&buffer[..read]).map_err(|_| fail("임시 파일을 쓰지 못했어요."))?;
    }
    file.sync_all().map_err(|_| fail("임시 파일을 쓰지 못했어요."))?;
    if written != plan.size {
        return Err(fail("내려받은 크기가 manifest와 달라요."));
    }
    if format!("{:x}", hasher.finalize()) != plan.sha256 {
        return Err(fail("내려받은 omp의 SHA-256이 manifest와 달라요."));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(temp, fs::Permissions::from_mode(0o755)).map_err(|_| fail("실행 권한을 설정하지 못했어요."))?;
    }
    Ok(())
}

/// Windows는 실행 중인 omp가 파일을 쥐고 있으면 교체할 수 없다. 프로세스를 닫지 않고 거절한다.
#[cfg(windows)]
fn ensure_not_in_use(target: &Path) -> Result<(), ApiError> {
    use std::os::windows::fs::OpenOptionsExt;
    fs::OpenOptions::new().write(true).share_mode(0).open(target).map(drop).map_err(|_| {
        error("OMP_UPDATE_IN_USE", "실행 중인 omp가 있어 교체하지 않았어요. 모든 omp를 종료한 뒤 다시 실행해 주세요. 프로세스는 닫지 않았어요.")
    })
}
#[cfg(not(windows))]
fn ensure_not_in_use(_: &Path) -> Result<(), ApiError> {
    Ok(())
}

fn swap(target: &Path, temp: &Path, backup: &Path) -> Result<(), ApiError> {
    let fail = |message: &str| error("OMP_UPDATE_REPLACE_FAILED", format!("{message} 기존 omp는 그대로 뒀어요."));
    #[cfg(unix)]
    {
        // 하드 링크 백업 뒤 rename으로 덮어쓰면 교체 순간에도 `target`이 항상 유효한 파일이다.
        fs::hard_link(target, backup).or_else(|_| fs::copy(target, backup).map(drop)).map_err(|_| fail("백업을 만들지 못했어요."))?;
        if fs::rename(temp, target).is_err() {
            let _ = fs::remove_file(backup);
            return Err(fail("실행 파일을 교체하지 못했어요."));
        }
    }
    #[cfg(windows)]
    {
        fs::rename(target, backup).map_err(|_| fail("백업을 만들지 못했어요."))?;
        if fs::rename(temp, target).is_err() {
            let restored = fs::rename(backup, target).is_ok();
            return Err(fail(if restored { "실행 파일을 교체하지 못했어요." } else { "실행 파일을 교체하지 못했고 백업 복원도 실패했어요. 백업 파일을 직접 확인해 주세요." }));
        }
    }
    Ok(())
}

fn rollback(target: &Path, backup: &Path) -> bool {
    #[cfg(windows)]
    let _ = fs::remove_file(target);
    fs::rename(backup, target).is_ok()
}

/// 플랜대로 `target`을 교체한다. 호출자는 서비스 버전을 이미 확인했다.
pub(crate) fn install(source: &Source, plan: &Plan, target: &Path) -> Result<(), ApiError> {
    ensure_not_in_use(target)?;
    let temp = sibling(target, "new");
    let backup = sibling(target, "bak");
    if let Err(problem) = download(source, plan, &temp) {
        let _ = fs::remove_file(&temp);
        return Err(problem);
    }
    if let Err(problem) = ensure_not_in_use(target).and_then(|_| swap(target, &temp, &backup)) {
        let _ = fs::remove_file(&temp);
        return Err(problem);
    }
    let reported = run_version(target);
    if reported.as_deref() != Some(plan.version.as_str()) {
        let restored = rollback(target, &backup);
        let _ = fs::remove_file(&temp);
        return Err(error("OMP_UPDATE_VERIFY_FAILED", format!(
            "교체한 omp가 {}을(으로) 보고하지 않았어요(보고: {}). {}",
            plan.version,
            reported.as_deref().unwrap_or("없음"),
            if restored { "기존 omp로 되돌렸어요." } else { "기존 omp 복원도 실패했어요. 백업 파일을 직접 확인해 주세요." },
        )));
    }
    let _ = fs::remove_file(&backup);
    Ok(())
}

/// 설치된 omp 파일. 심볼릭 링크는 실제 파일을 교체한다. 일반 파일이 아니거나 스크립트면 거절한다.
pub(crate) fn resolve_target(path: &Path) -> Result<PathBuf, ApiError> {
    let real = fs::canonicalize(path).map_err(|_| error("OMP_UPDATE_TARGET_MISSING", format!("교체할 omp를 찾지 못했어요: {}. 먼저 Ojak 수정본 omp를 설치해 주세요.", path.display())))?;
    if !fs::metadata(&real).is_ok_and(|meta| meta.is_file()) || !native_executable(&real) {
        return Err(error("OMP_UPDATE_TARGET_INVALID", format!("{}은(는) 네이티브 omp 실행 파일이 아니라서 교체하지 않았어요.", real.display())));
    }
    Ok(real)
}

fn newer(available: &str, installed: &str) -> bool {
    match (fork_version(available), fork_version(installed)) {
        (Some(available), Some(installed)) => available > installed,
        // 설치본이 Ojak 포크 표식이 없으면(일반 배포판) 포크를 올려 준다.
        (Some(_), None) => true,
        _ => false,
    }
}

pub(crate) struct Request<'a> {
    pub source: &'a Source,
    pub platform: &'a str,
    /// `resolve_target`이 확인한 실제 omp 파일.
    pub target: &'a Path,
    pub check: bool,
    pub force: bool,
    /// 실행 중인 서비스가 보고한 버전. 읽지 못했으면 `None`.
    pub service_version: Option<String>,
}

pub(crate) fn run(request: Request<'_>) -> Result<Report, ApiError> {
    let plan = plan(request.source, request.platform)?;
    let target = request.target.to_path_buf();
    let installed = run_version(&target);
    let report = |outcome| Report {
        outcome,
        installed: installed.clone(),
        available: plan.version.clone(),
        upstream_version: None,
        upstream_status: UpstreamStatus::Unknown,
        upstream_notice: "원본 최신 버전은 확인하지 못했어요. 호환 채널 기준 결과입니다.".into(),
        service_version: request.service_version.clone(),
        minimum_ojak_service: plan.minimum_ojak_service.clone(),
        path: target.display().to_string(),
    };
    let current = match installed.as_deref() {
        Some(version) => !request.force && !newer(&plan.version, version),
        None if request.force => false,
        None => return Err(error("OMP_UPDATE_TARGET_INVALID", "설치된 omp의 버전을 읽지 못했어요. 파일을 확인하거나 --force로 다시 설치해 주세요.")),
    };
    if current {
        return Ok(report(Outcome::UpToDate));
    }
    if !service_meets(request.service_version.as_deref(), &plan.minimum_ojak_service) {
        let seen = request.service_version.as_deref().map_or("서비스 버전을 읽지 못했어요".to_owned(), |version| format!("실행 중인 서비스 {version}"));
        return Err(error("OMP_UPDATE_OJAK_TOO_OLD", format!(
            "먼저 Ojak 앱을 업데이트해 주세요. 이 omp({})는 Ojak 서비스 {} 이상이 필요해요({seen}). omp는 바꾸지 않았어요.",
            plan.version, plan.minimum_ojak_service,
        )));
    }
    if request.check {
        return Ok(report(Outcome::WouldUpdate));
    }
    install(request.source, &plan, &target)?;
    Ok(report(Outcome::Updated))
}

/// 운영 진입점: 이 플랫폼의 omp를 GitHub omp 채널(`omp-v*` 릴리스)의 최신 유효 버전으로 올린다.
pub fn update(paths: &aam_protocol::Paths, check: bool, force: bool) -> Result<Report, ApiError> {
    let platform = HOST_PLATFORM.ok_or_else(|| error("OMP_UPDATE_PLATFORM_UNSUPPORTED", "이 플랫폼용 호환 omp 업데이트는 지원하지 않아요."))?;
    let service_version = aam_protocol::call(paths, "status.read", serde_json::json!({ "observe": false }))
        .ok()
        .and_then(|snapshot| snapshot.get("serviceVersion").and_then(Value::as_str).map(str::to_owned));
    let target = resolve_target(&crate::omp_bridge::omp()?)?;
    let mut report = run(Request { source: &Source::github(), platform, target: &target, check, force, service_version })?;
    report_upstream(&mut report, UPSTREAM_RELEASE_API);
    Ok(report)
}

/// 원본 조회는 안내용이다. 실패해도 검증된 호환판 설치를 되돌리거나 원본 실행 파일로 우회하지 않는다.
fn report_upstream(report: &mut Report, url: &str) {
    let latest = metadata_client()
        .and_then(|client| bounded(&client, url, RELEASE_LIMIT, None, Duration::from_secs(10)))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .and_then(|release| {
            if release["draft"] != false || release["prerelease"] != false { return None; }
            let tag = release["tag_name"].as_str()?.strip_prefix('v')?;
            if tag.contains(['-', '+']) || tag != tag.trim() { return None; }
            triple(tag)?;
            Some(tag.to_owned())
        });
    report.upstream_status = match latest.as_deref().and_then(triple) {
        Some(upstream) if triple(&report.available).is_some_and(|compatible| upstream > compatible) => UpstreamStatus::CompatibilityPending,
        Some(_) => UpstreamStatus::Current,
        None => UpstreamStatus::Unknown,
    };
    report.upstream_notice = match report.upstream_status {
        UpstreamStatus::CompatibilityPending => format!("원본 최신은 {}이고, 공개된 Ojak 호환판은 {}입니다. 호환 채널이 원본 최신판보다 뒤처져 있어 새 원본의 호환 빌드·검증·공개가 필요합니다.", latest.as_deref().unwrap(), report.available),
        UpstreamStatus::Current => "Ojak 호환판이 원본 최신 버전을 반영하고 있습니다.".into(),
        UpstreamStatus::Unknown => "원본 최신 버전 조회에 실패했어요. 호환 채널 기준 결과이며 원본 최신 여부는 알 수 없습니다.".into(),
    };
    report.upstream_version = latest;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        collections::HashMap,
        io::{BufRead, BufReader},
        net::TcpListener,
        sync::mpsc,
    };

    const OLD_VERSION: &str = "18.12.0-ojak.1";
    const NEW_VERSION: &str = "18.15.0-ojak.1";
    const DARWIN: &str = "omp-v2-darwin-arm64";

    type Routes = HashMap<String, (u16, Vec<(String, String)>, Vec<u8>)>;

    /// 고정 응답을 주는 로컬 HTTP 서버. 받은 요청 경로는 채널로 기록한다.
    struct Server {
        port: u16,
        seen: mpsc::Receiver<String>,
    }
    impl Server {
        fn start(routes: impl FnOnce(u16) -> Routes) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            let routes = routes(port);
            let (log, seen) = mpsc::channel();
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    let Ok(mut stream) = stream else { continue };
                    let mut reader = BufReader::new(stream.try_clone().unwrap());
                    let mut line = String::new();
                    if reader.read_line(&mut line).is_err() {
                        continue;
                    }
                    loop {
                        let mut header = String::new();
                        if reader.read_line(&mut header).unwrap_or(0) <= 2 {
                            break;
                        }
                    }
                    let path = line.split_whitespace().nth(1).unwrap_or("").to_owned();
                    let _ = log.send(path.clone());
                    let (status, headers, body) = routes.get(&path).cloned().unwrap_or((404, vec![], b"missing".to_vec()));
                    let mut head = format!("HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n", body.len());
                    for (name, value) in headers {
                        head.push_str(&format!("{name}: {value}\r\n"));
                    }
                    head.push_str("\r\n");
                    let _ = stream.write_all(head.as_bytes());
                    let _ = stream.write_all(&body);
                }
            });
            Self { port, seen }
        }
        fn source(&self) -> Source {
            Source {
                api: format!("http://127.0.0.1:{}/api/releases", self.port),
                download_prefix: format!("http://127.0.0.1:{}/download/", self.port),
                redirect_scheme: "http",
                redirect_host: "127.0.0.1".into(),
            }
        }
        fn paths(&self) -> Vec<String> {
            self.seen.try_iter().collect()
        }
    }

    /// `omp --version`만 흉내 내는 실행 파일. Unix는 쉘 스크립트, Windows는 같은 이름으로 실행되는 배치 파일이라
    /// 파일 이름(`omp`·`omp.cmd`)이 플랫폼마다 다르다. 네이티브 검사는 `resolve_target`에서만 하므로 여기서는 통과시킨다.
    fn fake_omp(version: &str) -> Vec<u8> {
        if cfg!(windows) {
            format!("@echo omp/{version}\r\n").into_bytes()
        } else {
            format!("#!/bin/sh\necho 'omp/{version}'\n").into_bytes()
        }
    }

    struct Fixture {
        dir: PathBuf,
    }
    impl Fixture {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!("ojak-omp-update-{}", aam_protocol::new_id()));
            fs::create_dir_all(&dir).unwrap();
            Self { dir: dir.canonicalize().unwrap() }
        }
        fn omp(&self, version: &str) -> PathBuf {
            let path = self.dir.join(if cfg!(windows) { "omp.cmd" } else { "omp" });
            fs::write(&path, fake_omp(version)).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
            }
            path
        }
        fn leftovers(&self) -> usize {
            fs::read_dir(&self.dir).unwrap().count()
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    const TAG: &str = "omp-v18.15.0-ojak.1";
    const APP_TAG: &str = "v0.22.2";

    struct Release {
        binary: Vec<u8>,
        minimum: &'static str,
        /// manifest 버전.
        version: String,
        /// 릴리스 태그. 기본은 manifest 버전과 같은 `omp-v<version>`.
        tag: String,
        tamper: Option<&'static str>,
    }
    impl Release {
        fn new() -> Self {
            Self::versioned(NEW_VERSION)
        }
        /// 태그와 manifest 버전이 같은 정상 릴리스. 내려받는 실행 파일은 항상 `NEW_VERSION`을 보고한다.
        fn versioned(version: &str) -> Self {
            Self { binary: fake_omp(NEW_VERSION), minimum: "0.22.2", version: version.into(), tag: format!("omp-v{version}"), tamper: None }
        }
        fn base(&self, port: u16) -> String {
            format!("http://127.0.0.1:{port}/download/{}/", self.tag)
        }
        /// 릴리스 목록 항목과 그 릴리스의 manifest 바이트.
        fn entry(&self, port: u16) -> (Value, Vec<u8>) {
            let digest = format!("{:x}", Sha256::digest(&self.binary));
            let entry = |name: &str| serde_json::json!({ "asset": name, "size": self.binary.len(), "sha256": digest });
            let mut manifest = serde_json::json!({
                "schemaVersion": 2, "admissionProtocol": 1, "version": self.version, "minimumOjakService": self.minimum,
                "platforms": { "darwin-arm64": entry(DARWIN), "win32-x64": entry("omp-v2-windows-x64.exe") },
            });
            let base = self.base(port);
            let mut assets = vec![
                serde_json::json!({ "name": MANIFEST, "size": 0, "browser_download_url": format!("{base}{MANIFEST}") }),
                serde_json::json!({ "name": DARWIN, "size": self.binary.len(), "browser_download_url": format!("{base}{DARWIN}") }),
                serde_json::json!({ "name": "omp-v2-windows-x64.exe", "size": self.binary.len(), "browser_download_url": format!("{base}omp-v2-windows-x64.exe") }),
            ];
            match self.tamper {
                Some("schema1") => manifest["schemaVersion"] = 1.into(),
                Some("minimum") => manifest["minimumOjakService"] = "latest".into(),
                Some("size") => assets[1]["size"] = (self.binary.len() + 1).into(),
                Some("missing-manifest") => {
                    assets.remove(0);
                }
                Some("old-assets") => {
                    assets = vec![
                        serde_json::json!({ "name": "omp-compatible.json", "size": 10, "browser_download_url": format!("{base}omp-compatible.json") }),
                        serde_json::json!({ "name": "omp-darwin-arm64", "size": 10, "browser_download_url": format!("{base}omp-darwin-arm64") }),
                    ];
                }
                _ => {}
            }
            let manifest_bytes = manifest.to_string().into_bytes();
            if let Some(asset) = assets.iter_mut().find(|asset| asset["name"] == MANIFEST) {
                asset["size"] = manifest_bytes.len().into();
            }
            let mut release = serde_json::json!({ "draft": false, "prerelease": false, "published_at": "2026-11-03T00:00:00Z", "tag_name": self.tag, "assets": assets });
            match self.tamper {
                Some("draft") => release["draft"] = true.into(),
                Some("prerelease") => release["prerelease"] = true.into(),
                _ => {}
            }
            (release, manifest_bytes)
        }
        /// 이 릴리스가 서빙하는 manifest·바이너리 경로.
        fn files(&self, port: u16, manifest: Vec<u8>) -> Routes {
            let base = self.base(port);
            let path = |name: &str| base.strip_prefix(&format!("http://127.0.0.1:{port}")).unwrap().to_owned() + name;
            let served = if self.tamper == Some("digest") { vec![0u8; self.binary.len()] } else { self.binary.clone() };
            HashMap::from([
                (path(MANIFEST), (200, vec![], manifest)),
                (path(DARWIN), (200, vec![], served.clone())),
                (path("omp-v2-windows-x64.exe"), (200, vec![], served)),
            ])
        }
        fn routes(&self, port: u16) -> Routes {
            Self::channel(port, &[self])
        }
        /// `/releases` 목록 = 앱 릴리스 A(latest, omp asset 없음) + 주어진 omp 릴리스들. `/releases/latest`는 A를 돌려준다.
        fn channel(port: u16, releases: &[&Release]) -> Routes {
            let app = app_release(port);
            let mut list = vec![app.clone()];
            let mut routes = HashMap::from([("/api/releases/latest".to_owned(), (200, vec![], app.to_string().into_bytes()))]);
            for release in releases {
                let (entry, manifest) = release.entry(port);
                list.push(entry);
                routes.extend(release.files(port, manifest));
            }
            routes.insert("/api/releases".to_owned(), (200, vec![], Value::Array(list).to_string().into_bytes()));
            routes
        }
        fn serve(&self) -> Server {
            Server::start(|port| self.routes(port))
        }
    }

    /// 앱 릴리스 A: 공개·latest이고 `latest.json`을 싣지만 omp asset은 없다.
    fn app_release(port: u16) -> Value {
        let base = format!("http://127.0.0.1:{port}/download/{APP_TAG}/");
        serde_json::json!({ "draft": false, "prerelease": false, "published_at": "2026-11-04T00:00:00Z", "tag_name": APP_TAG, "assets": [
            { "name": "latest.json", "size": 400, "browser_download_url": format!("{base}latest.json") },
            { "name": "Ojak.app.tar.gz", "size": 4000, "browser_download_url": format!("{base}Ojak.app.tar.gz") },
        ] })
    }

    fn request<'a>(source: &'a Source, target: &'a Path, check: bool, force: bool, service: Option<&str>) -> Request<'a> {
        Request { source, platform: "darwin-arm64", target, check, force, service_version: service.map(str::to_owned) }
    }
    fn downloaded_binary(server: &Server) -> bool {
        server.paths().iter().any(|path| path.ends_with(DARWIN))
    }

    #[test]
    fn service_gate_is_numeric_and_fails_closed() {
        assert!(service_meets(Some("0.22.2"), "0.22.2"));
        assert!(service_meets(Some("0.22.10"), "0.22.2"));
        assert!(service_meets(Some("1.0.0"), "0.22.2"));
        assert!(service_meets(Some("0.23.0-rc.1+b5"), "0.22.2"));
        assert!(!service_meets(Some("0.22.1"), "0.22.2"));
        assert!(!service_meets(Some("0.9.9"), "0.22.2"));
        for bad in [None, Some(""), Some("0.22"), Some("0.22.2.1"), Some("a.b.c"), Some("01.2.3")] {
            assert!(!service_meets(bad, "0.22.2"), "{bad:?}");
        }
        assert!(!service_meets(Some("0.22.2"), "bad"));
    }

    #[test]
    fn fork_versions_order_by_base_then_revision() {
        assert!(newer("18.15.0-ojak.1", "18.12.0-ojak.2"));
        assert!(newer("18.15.0-ojak.2", "18.15.0-ojak.1"));
        assert!(!newer("18.15.0-ojak.1", "18.15.0-ojak.1"));
        assert!(!newer("18.12.0-ojak.9", "18.15.0-ojak.1"));
        assert!(newer("18.15.0-ojak.1", "18.15.0"));
        assert!(!newer("18.15.0", "18.12.0-ojak.1"));
    }

    #[test]
    fn current_service_installs_verified_bytes_and_cleans_up() {
        let fixture = Fixture::new();
        let target = fixture.omp(OLD_VERSION);
        let release = Release::new();
        let server = release.serve();
        let report = run(request(&server.source(), &target, false, false, Some("0.22.2"))).unwrap();
        assert_eq!(report.outcome, Outcome::Updated);
        assert_eq!(report.installed.as_deref(), Some(OLD_VERSION));
        assert_eq!(fs::read(&target).unwrap(), release.binary);
        assert_eq!(fixture.leftovers(), 1, "temp and backup files must not linger");
        assert_eq!(server.paths(), ["/api/releases", &format!("/download/{TAG}/{MANIFEST}"), &format!("/download/{TAG}/{DARWIN}")]);
    }

    #[test]
    fn older_unknown_or_malformed_service_refuses_before_downloading() {
        for service in [Some("0.22.1"), Some("0.1.0"), Some(""), Some("junk"), None] {
            for check in [false, true] {
                let fixture = Fixture::new();
                let target = fixture.omp(OLD_VERSION);
                let before = fs::read(&target).unwrap();
                let server = Release::new().serve();
                let problem = run(request(&server.source(), &target, check, true, service)).unwrap_err();
                assert_eq!(problem.code, "OMP_UPDATE_OJAK_TOO_OLD", "{service:?}");
                assert!(problem.message.contains("Ojak 앱을 업데이트"));
                assert_eq!(fs::read(&target).unwrap(), before);
                assert!(!downloaded_binary(&server), "binary must not be requested");
            }
        }
    }

    #[test]
    fn check_reports_without_changing_the_file_and_up_to_date_is_a_no_op() {
        let fixture = Fixture::new();
        let target = fixture.omp(OLD_VERSION);
        let before = fs::read(&target).unwrap();
        let server = Release::new().serve();
        let report = run(request(&server.source(), &target, true, false, Some("0.22.2"))).unwrap();
        assert_eq!((report.outcome, report.available.as_str()), (Outcome::WouldUpdate, NEW_VERSION));
        assert_eq!(fs::read(&target).unwrap(), before);
        assert!(!downloaded_binary(&server));
        let current = fixture.omp(NEW_VERSION);
        let server = Release::new().serve();
        assert_eq!(run(request(&server.source(), &current, false, false, None)).unwrap().outcome, Outcome::UpToDate);
        assert!(!downloaded_binary(&server));
    }

    #[test]
    fn version_mismatch_after_replacement_rolls_back() {
        let fixture = Fixture::new();
        let target = fixture.omp(OLD_VERSION);
        let before = fs::read(&target).unwrap();
        let release = Release::versioned("18.15.0-ojak.9"); // 내려받는 실행 파일은 ojak.1을 보고한다
        let server = release.serve();
        let problem = run(request(&server.source(), &target, false, false, Some("0.22.2"))).unwrap_err();
        assert_eq!(problem.code, "OMP_UPDATE_VERIFY_FAILED");
        assert!(problem.message.contains("되돌렸어요"));
        assert_eq!(fs::read(&target).unwrap(), before);
        assert_eq!(fixture.leftovers(), 1);
    }

    /// Native release gate: use copies, never either supplied executable in place.
    #[test]
    #[ignore = "requires AAM_OMP_NATIVE_OLD and AAM_OMP_NATIVE_NEW for the current host"]
    fn native_binary_update_and_rollback() {
        let old = PathBuf::from(std::env::var_os("AAM_OMP_NATIVE_OLD").expect("AAM_OMP_NATIVE_OLD"));
        let new = PathBuf::from(std::env::var_os("AAM_OMP_NATIVE_NEW").expect("AAM_OMP_NATIVE_NEW"));
        resolve_target(&old).unwrap();
        resolve_target(&new).unwrap();
        let old_version = run_version(&old).unwrap();
        let new_version = run_version(&new).unwrap();
        assert_ne!(old_version, new_version, "release gate requires different native builds");
        let platform = if cfg!(windows) { "win32-x64" } else { "darwin-arm64" };
        let fixture = Fixture::new();
        let target = fixture.dir.join(if cfg!(windows) { "omp.exe" } else { "omp" });
        fs::copy(&old, &target).unwrap();
        let before = Sha256::digest(fs::read(&target).unwrap());
        let mut release = Release::versioned(&new_version);
        release.binary = fs::read(&new).unwrap();
        let expected = Sha256::digest(&release.binary);
        let server = release.serve();
        let source = server.source();
        let report = run(Request {
            source: &source, platform, target: &target, check: false, force: true,
            service_version: Some(env!("CARGO_PKG_VERSION").into()),
        }).unwrap();
        assert_eq!(report.outcome, Outcome::Updated);
        assert_eq!(run_version(&target).as_deref(), Some(new_version.as_str()));
        assert_eq!(Sha256::digest(fs::read(&target).unwrap()), expected);
        assert_eq!(fixture.leftovers(), 1);

        fs::copy(&old, &target).unwrap();
        // Valid metadata and digest, but the real binary reports a different version.
        release.version = "999.0.0-ojak.1".into();
        release.tag = "omp-v999.0.0-ojak.1".into();
        let server = release.serve();
        let source = server.source();
        let problem = run(Request {
            source: &source, platform, target: &target, check: false, force: true,
            service_version: Some(env!("CARGO_PKG_VERSION").into()),
        }).unwrap_err();
        assert_eq!(problem.code, "OMP_UPDATE_VERIFY_FAILED");
        assert_eq!(run_version(&target).as_deref(), Some(old_version.as_str()));
        assert_eq!(Sha256::digest(fs::read(&target).unwrap()), before);
        assert_eq!(fixture.leftovers(), 1);
        println!("native update {old_version} -> {new_version} and exact rollback passed on {platform}");
    }

    #[test]
    fn invalid_release_shapes_never_touch_the_executable() {
        for tamper in ["schema1", "minimum", "size", "missing-manifest", "old-assets", "draft", "prerelease", "digest"] {
            let fixture = Fixture::new();
            let target = fixture.omp(OLD_VERSION);
            let before = fs::read(&target).unwrap();
            let mut release = Release::new();
            release.tamper = Some(tamper);
            let server = release.serve();
            assert!(run(request(&server.source(), &target, false, true, Some("9.9.9"))).is_err(), "{tamper}");
            assert_eq!(fs::read(&target).unwrap(), before, "{tamper}");
            assert_eq!(fixture.leftovers(), 1, "{tamper}");
            if tamper != "digest" {
                assert!(!downloaded_binary(&server), "{tamper}: binary must not be requested");
            }
        }
    }

    #[test]
    fn only_the_github_asset_host_is_followed_on_redirect() {
        let fixture = Fixture::new();
        let target = fixture.omp(OLD_VERSION);
        let release = Release::new();
        let server = Server::start(|port| {
            let mut routes = release.routes(port);
            let real = format!("/download/{TAG}/{DARWIN}");
            routes.insert("/to-allowed".into(), (302, vec![("Location".into(), format!("http://127.0.0.1:{port}{real}"))], vec![]));
            routes.insert("/to-other-host".into(), (302, vec![("Location".into(), format!("http://localhost:{port}{real}"))], vec![]));
            routes
        });
        let source = server.source();
        let plan = plan(&source, "darwin-arm64").unwrap();
        let refused = install(&source, &Plan { url: format!("http://127.0.0.1:{}/to-other-host", server.port), ..plan.clone() }, &target).unwrap_err();
        assert_eq!(refused.code, "OMP_UPDATE_NETWORK");
        assert_eq!(fs::read(&target).unwrap(), fake_omp(OLD_VERSION));
        assert_eq!(fixture.leftovers(), 1);
        install(&source, &Plan { url: format!("http://127.0.0.1:{}/to-allowed", server.port), ..plan }, &target).unwrap();
        assert_eq!(fs::read(&target).unwrap(), release.binary);
    }

    #[test]
    fn scripts_and_missing_files_are_not_replacement_targets() {
        let fixture = Fixture::new();
        assert_eq!(resolve_target(&fixture.dir.join("missing")).unwrap_err().code, "OMP_UPDATE_TARGET_MISSING");
        assert_eq!(resolve_target(&fixture.omp(OLD_VERSION)).unwrap_err().code, "OMP_UPDATE_TARGET_INVALID");
    }

    fn paths_without_api(server: &Server) -> Vec<String> {
        server.paths().into_iter().filter(|path| !path.starts_with("/api/")).collect()
    }
    fn serve_channel(releases: Vec<Release>) -> Server {
        Server::start(move |port| Release::channel(port, &releases.iter().collect::<Vec<_>>()))
    }

    #[test]
    fn finds_the_non_latest_omp_release_while_latest_stays_the_app_release() {
        let fixture = Fixture::new();
        let target = fixture.omp(OLD_VERSION);
        let release = Release::new(); // B: omp-v18.15.0-ojak.1, 공개이지만 latest 아님
        let server = release.serve();
        let plan = plan(&server.source(), "darwin-arm64").unwrap();
        assert_eq!(plan.version, NEW_VERSION);
        assert!(plan.url.contains(&format!("/download/{TAG}/")), "{}", plan.url);
        let requested = server.paths();
        assert_eq!(requested[0], "/api/releases");
        assert!(!requested.iter().any(|path| path == "/api/releases/latest"), "{requested:?}");
        // 앱 업데이터가 쓰는 `/releases/latest`는 여전히 A(앱 릴리스, omp asset 없음)다.
        let latest = bounded(&metadata_client().unwrap(), &format!("http://127.0.0.1:{}/api/releases/latest", server.port), RELEASE_LIMIT, None, Duration::from_secs(5)).unwrap();
        let latest: Value = serde_json::from_slice(&latest).unwrap();
        assert_eq!(latest["tag_name"], APP_TAG);
        assert!(latest["assets"].as_array().unwrap().iter().all(|asset| !asset["name"].as_str().unwrap().starts_with("omp-")));
        assert_eq!(run(request(&server.source(), &target, true, false, Some("0.22.2"))).unwrap().outcome, Outcome::WouldUpdate);
    }

    #[test]
    fn picks_the_highest_valid_omp_release_and_skips_app_releases() {
        let server = serve_channel(vec![Release::versioned("18.15.0-ojak.2"), Release::versioned("18.15.0-ojak.10"), Release::versioned("18.15.0-ojak.9")]);
        assert_eq!(plan(&server.source(), "win32-x64").unwrap().version, "18.15.0-ojak.10");
        assert_eq!(paths_without_api(&server), [format!("/download/omp-v18.15.0-ojak.10/{MANIFEST}")]);
    }

    #[test]
    fn draft_or_prerelease_omp_releases_are_ignored_in_favor_of_the_next_valid_one() {
        for tamper in ["draft", "prerelease"] {
            let mut newer = Release::versioned("18.15.0-ojak.2");
            newer.tamper = Some(tamper);
            let server = serve_channel(vec![newer, Release::new()]);
            assert_eq!(plan(&server.source(), "darwin-arm64").unwrap().version, NEW_VERSION, "{tamper}");
            assert_eq!(paths_without_api(&server), [format!("/download/{TAG}/{MANIFEST}")], "{tamper}: the ignored release's manifest must not be requested");
        }
        let mut only = Release::new();
        only.tamper = Some("draft");
        let server = serve_channel(vec![only]);
        assert_eq!(plan(&server.source(), "darwin-arm64").unwrap_err().code, "OMP_UPDATE_RELEASE_INVALID");
        assert!(paths_without_api(&server).is_empty());
    }

    #[test]
    fn malformed_omp_tags_are_ignored_without_requesting_their_manifest() {
        for tag in ["omp-v18.15.0", "omp-v18.15.0-ojak.0", "omp-v18.15.0-ojak.01", "omp-18.15.0-ojak.1", "omp-v18.15-ojak.1", "omp-v18.15.0-ojak.1-rc", "omp-v 18.15.0-ojak.1", "v18.15.0-ojak.1"] {
            let mut release = Release::new();
            release.tag = tag.into();
            let server = serve_channel(vec![release]);
            let problem = plan(&server.source(), "darwin-arm64").unwrap_err();
            assert_eq!(problem.code, "OMP_UPDATE_RELEASE_INVALID", "{tag}");
            assert!(paths_without_api(&server).is_empty(), "{tag}: {:?}", server.paths());
        }
    }

    #[test]
    fn manifest_version_that_differs_from_the_tag_is_ignored() {
        let mut mismatched = Release::versioned("18.15.0-ojak.3"); // manifest 18.15.0-ojak.3
        mismatched.tag = "omp-v18.15.0-ojak.2".into(); // 태그는 ojak.2
        let server = serve_channel(vec![mismatched, Release::new()]);
        assert_eq!(plan(&server.source(), "darwin-arm64").unwrap().version, NEW_VERSION);
        let mut only = Release::versioned("18.15.0-ojak.3");
        only.tag = "omp-v18.15.0-ojak.2".into();
        let server = serve_channel(vec![only]);
        let problem = plan(&server.source(), "darwin-arm64").unwrap_err();
        assert_eq!(problem.code, "OMP_UPDATE_RELEASE_INVALID");
        assert!(problem.message.contains("태그 버전"), "{}", problem.message);
    }

    #[test]
    fn an_empty_omp_channel_never_falls_back_to_the_app_release() {
        let server = serve_channel(vec![]);
        assert_eq!(plan(&server.source(), "darwin-arm64").unwrap_err().code, "OMP_UPDATE_RELEASE_INVALID");
        assert!(paths_without_api(&server).is_empty());
    }

    #[test]
    fn manifest_requests_are_bounded_for_a_polluted_channel() {
        let releases = (1..=12).map(|n| {
            let mut release = Release::versioned(&format!("18.15.0-ojak.{n}"));
            release.version = format!("18.15.0-ojak.{}", n + 100);
            release
        }).collect();
        let server = serve_channel(releases);
        assert!(plan(&server.source(), "darwin-arm64").is_err());
        assert_eq!(paths_without_api(&server).len(), MAX_MANIFEST_CHECKS);
    }

    #[test]
    fn channel_release_not_newer_than_the_installed_omp_changes_nothing() {
        for installed in [NEW_VERSION, "18.15.0-ojak.7"] {
            let fixture = Fixture::new();
            let target = fixture.omp(installed);
            let before = fs::read(&target).unwrap();
            let server = Release::new().serve();
            assert_eq!(run(request(&server.source(), &target, false, false, None)).unwrap().outcome, Outcome::UpToDate, "{installed}");
            assert_eq!(fs::read(&target).unwrap(), before);
            assert!(!downloaded_binary(&server));
        }
    }

    #[test]
    fn upstream_lag_and_unknown_are_not_reported_as_current() {
        let fixture = Fixture::new();
        let target = fixture.omp(NEW_VERSION);
        let before = fs::read(&target).unwrap();
        let server = Release::new().serve();
        let mut report = run(request(&server.source(), &target, true, false, None)).unwrap();
        for (tag, draft, prerelease, expected, version) in [
            ("v18.18.9", false, false, UpstreamStatus::CompatibilityPending, Some("18.18.9")),
            ("v18.15.0", false, false, UpstreamStatus::Current, Some("18.15.0")),
            ("v18.18.9-rc.1", false, false, UpstreamStatus::Unknown, None),
            ("v18.18.9", true, false, UpstreamStatus::Unknown, None),
            ("v18.18.9", false, true, UpstreamStatus::Unknown, None),
            ("v18.018.9", false, false, UpstreamStatus::Unknown, None),
        ] {
            let upstream = Server::start(|_| HashMap::from([("/latest".into(), (200, vec![], serde_json::json!({"tag_name":tag,"draft":draft,"prerelease":prerelease}).to_string().into_bytes()))]));
            report_upstream(&mut report, &format!("http://127.0.0.1:{}/latest", upstream.port));
            assert_eq!(report.upstream_status, expected, "{tag}");
            assert_eq!(report.upstream_version.as_deref(), version);
            assert_eq!(report.outcome, Outcome::UpToDate); // 호환 채널 판정과 원본 판정은 별개다.
            assert_eq!(fs::read(&target).unwrap(), before);
        }
        report_upstream(&mut report, &format!("http://127.0.0.1:{}/missing", server.port));
        assert_eq!(report.upstream_status, UpstreamStatus::Unknown);
        assert_eq!(report.upstream_version, None);
        assert_eq!(fs::read(&target).unwrap(), before);
    }
}
