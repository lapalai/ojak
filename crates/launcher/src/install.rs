// Platform-specific installation helpers share the owned manifests and native entry-path contract.
#![cfg_attr(windows, allow(dead_code, unused_imports))]
use aam_protocol::{call, ApiError, Paths, Snapshot, INTEGRATION_VERSION};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    collections::BTreeMap,
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};
#[cfg(unix)]
use std::{
    os::unix::fs::{symlink, MetadataExt, OpenOptionsExt, PermissionsExt},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

const OWNER: &str = "ai-account-manager";
const LABEL: &str = "ai.aam.service";
const TOOLS: [&str; 2] = ["claude", "codex"];
/// 이전 판이 등록했던 이름입니다. 설치·제거 때 앱 소유 링크만 정리하고 다시 만들지 않습니다.
const LEGACY_TOOLS: [&str; 3] = ["grok", "agy", "omp"];

#[cfg(unix)]
#[path = "shell.rs"]
mod shell;
#[cfg(unix)]
pub(crate) use shell::shell_configured;
#[cfg(unix)]
pub use shell::{shell_install, shell_uninstall};

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Integration {
    owner: String,
    #[serde(default)]
    version: u32,
    launcher_path: PathBuf,
    /// 원본 CLI의 진입 경로(symlink 포함)입니다. 실행 대상은 쓸 때마다 해석합니다.
    native_binaries: BTreeMap<String, String>,
    shims: Vec<String>,
}
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ServiceInstall {
    owner: String,
    binary_path: PathBuf,
    plist_path: PathBuf,
    plist_contents: String,
    #[serde(default)]
    uninstall_permit: Option<String>,
}

fn error(code: &str, text: &str) -> ApiError {
    ApiError::new(code, text)
}
fn io_error(_: std::io::Error) -> ApiError {
    error(
        "INSTALL_IO_ERROR",
        "설치 파일을 처리하지 못했어요. 폴더 소유와 쓰기 권한을 확인해 주세요.",
    )
}
fn platform() -> Result<(), ApiError> {
    if cfg!(target_os = "macos") {
        Ok(())
    } else {
        Err(error(
            "UNSUPPORTED_PLATFORM",
            "서비스와 명령 연결 설치는 지금 macOS만 지원해요.",
        ))
    }
}

pub(crate) fn private_dir(path: &Path) -> Result<(), ApiError> {
    if !path.is_absolute() {
        return Err(error(
            "INVALID_PATH",
            "앱 폴더는 절대 경로여야 해요.",
        ));
    }
    fs::create_dir_all(path).map_err(io_error)?;
    let metadata = fs::symlink_metadata(path).map_err(io_error)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() || !owned_by_me(path, &metadata) {
        return Err(error(
            "UNSAFE_PATH",
            "앱 폴더의 소유나 종류가 맞지 않아요.",
        ));
    }
    aam_protocol::secure::restrict_dir(path).map_err(io_error)
}

/// 경로의 소유자가 현재 사용자인지(Unix uid, Windows 소유자 SID).
fn owned_by_me(path: &Path, metadata: &fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        let _ = path;
        metadata.uid() == unsafe { libc::geteuid() }
    }
    #[cfg(windows)]
    {
        let _ = metadata;
        aam_protocol::secure::owned_by_current_user(path).unwrap_or(false)
    }
}

fn read_owned<T: serde::de::DeserializeOwned>(path: &Path) -> Result<Option<T>, ApiError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(io_error(e)),
    };
    if !metadata.is_file()
        || metadata.file_type().is_symlink()
        || !owned_by_me(path, &metadata)
        || metadata.len() > 1024 * 1024
    {
        return Err(error(
            "UNSAFE_PATH",
            "설치 기록의 소유나 파일 형식이 맞지 않아요.",
        ));
    }
    serde_json::from_slice(&fs::read(path).map_err(io_error)?)
        .map(Some)
        .map_err(|_| {
            error(
                "INVALID_INSTALL_RECORD",
                "기존 설치 기록을 읽지 못했어요. 그 파일은 덮어쓰지 않았어요.",
            )
        })
}

/// A second app copy must not rewrite integrations owned by the installed helper.
pub fn is_current_launcher(paths: &Paths) -> Result<bool, ApiError> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct ManagedService {
        owner: String,
        binary_path: PathBuf,
    }
    let expected = if let Some(record) = read_owned::<ManagedService>(&paths.home.join("service-install.json"))? {
        if record.owner != OWNER {
            return Err(error("FOREIGN_INSTALL", "다른 프로그램의 서비스 연결은 바꾸지 않아요."));
        }
        record.binary_path.with_file_name(if cfg!(windows) { "aam.exe" } else { "aam" })
    } else if let Some(record) = read_owned::<Integration>(&paths.home.join("integration.json"))? {
        if record.owner != OWNER {
            return Err(error("FOREIGN_INSTALL", "다른 프로그램의 명령 연결은 바꾸지 않아요."));
        }
        record.launcher_path
    } else {
        return Ok(false);
    };
    let current = std::env::current_exe().and_then(fs::canonicalize).map_err(io_error)?;
    Ok(expected.canonicalize().is_ok_and(|path| path == current))
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), ApiError> {
    if fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_symlink()) {
        return Err(error("UNSAFE_PATH", "바로가기는 바꾸지 않아요."));
    }
    let name = path.file_name().and_then(|name| name.to_str()).unwrap_or("write");
    let temp = path.with_file_name(format!(".{name}.{}.tmp", aam_protocol::new_id()));
    let result = (|| {
        let mut file = aam_protocol::secure::private_options(OpenOptions::new().write(true).create_new(true))
            .open(&temp)
            .map_err(io_error)?;
        file.write_all(bytes).map_err(io_error)?;
        file.sync_all().map_err(io_error)?;
        drop(file);
        // Windows: 승격된 토큰(관리자 그룹 사용자의 SSH·설치 실행)은 새 파일의 기본 소유자를
        // BUILTIN\Administrators로 둔다. 그러면 일반 권한 실행에서 `read_owned`가 남의 파일로 거부한다.
        // 실행 토큰과 관계없이 현재 사용자 SID를 소유자로 명시하고 소유자 전용 ACL로 둔다.
        #[cfg(windows)]
        aam_protocol::secure::restrict_file(&temp).map_err(io_error)?;
        fs::rename(&temp, path).map_err(io_error)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}
fn save(path: &Path, value: &impl Serialize) -> Result<(), ApiError> {
    atomic_write(
        path,
        &serde_json::to_vec_pretty(value)
            .map_err(|_| error("INSTALL_SERIALIZATION", "설치 기록을 만들지 못했어요."))?,
    )
}

pub fn native_program(path: &Path) -> Result<PathBuf, ApiError> {
    let path = fs::canonicalize(path)
        .map_err(|_| error("BINARY_MISSING", "공식 실행 파일이 없어요."))?;
    let metadata = fs::metadata(&path).map_err(io_error)?;
    let own = std::env::current_exe()
        .and_then(fs::canonicalize)
        .map_err(io_error)?;
    if !metadata.is_file() || !runnable(&path, &metadata) || same_file(&path, &own)? {
        return Err(error(
            "RECURSIVE_BINARY",
            "실행 파일이 Ojak 자신이거나 실행할 수 없어요.",
        ));
    }
    Ok(path)
}

fn runnable(path: &Path, metadata: &fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        let _ = path;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(windows)]
    {
        let _ = metadata;
        path.extension()
            .and_then(|v| v.to_str())
            .is_some_and(|ext| ["exe", "cmd", "bat", "com"].contains(&ext.to_ascii_lowercase().as_str()))
    }
}

/// 같은 파일인지. Unix는 장치·inode, Windows는 정규화한 경로(하드 링크까지는 구분하지 않음)로 비교한다.
fn same_file(path: &Path, own: &Path) -> Result<bool, ApiError> {
    #[cfg(unix)]
    {
        let (a, b) = (fs::metadata(path).map_err(io_error)?, fs::metadata(own).map_err(io_error)?);
        Ok(a.dev() == b.dev() && a.ino() == b.ino())
    }
    #[cfg(windows)]
    {
        Ok(path == own)
    }
}

pub(crate) fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"))
}

pub(crate) fn registered_native(paths: &Paths, tool: &str) -> Result<PathBuf, ApiError> {
    let record: Integration =
        read_owned(&paths.home.join("integration.json"))?.ok_or_else(|| {
            error(
                "INTEGRATION_REQUIRED",
                "원래 CLI 등록이 없어요. 연결을 먼저 설치해 주세요.",
            )
        })?;
    if record.owner != OWNER || !TOOLS.contains(&tool) {
        return Err(error(
            "INVALID_INSTALL_RECORD",
            "이 앱이 등록한 도구만 쓸 수 있어요.",
        ));
    }
    let original = record
        .native_binaries
        .get(tool)
        .map(PathBuf::from)
        .ok_or_else(|| {
            error(
                "BINARY_MISSING",
                "등록된 원래 CLI가 없어요. 원래 CLI를 설치한 뒤 연결을 다시 설치해 주세요.",
            )
        })?;
    if !original.is_absolute() {
        return Err(error(
            "INVALID_INSTALL_RECORD",
            "등록된 원래 CLI 경로가 절대 경로가 아니에요. 연결을 다시 설치해 주세요.",
        ));
    }
    let verified = native_program(&original)?;
    let bin = paths.home.join("bin");
    if original.starts_with(&bin)
        || verified == record.launcher_path
        || verified.starts_with(&bin)
        || verified
            .file_name()
            .is_some_and(|name| name == "aam")
    {
        return Err(error(
            "RECURSIVE_BINARY",
            "등록된 원래 CLI가 Ojak 명령을 가리켜요.",
        ));
    }
    Ok(verified)
}

#[cfg(unix)]
pub(crate) fn shim_installed(paths: &Paths, tool: &str) -> Result<bool, ApiError> {
    let Some(record): Option<Integration> = read_owned(&paths.home.join("integration.json"))?
    else {
        return Ok(false);
    };
    if record.owner != OWNER {
        return Err(error(
            "FOREIGN_INSTALL",
            "이 앱 소유가 아닌 연결 기록이에요. 기존 파일은 그대로 두고 설치 경로를 확인해 주세요.",
        ));
    }
    Ok(record.shims.iter().any(|name| name == tool)
        && record.launcher_path.is_absolute()
        && fs::metadata(&record.launcher_path)
            .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
        && fs::read_link(paths.home.join("bin").join(tool))
            .ok()
            .as_ref()
            == Some(&record.launcher_path))
}

#[cfg(unix)]
pub fn integration_install(paths: &Paths) -> Result<String, ApiError> {
    platform()?;
    private_dir(&paths.home)?;
    let bin = paths.home.join("bin");
    private_dir(&bin)?;
    let record_path = paths.home.join("integration.json");
    let previous: Option<Integration> = read_owned(&record_path)?;
    if previous.as_ref().is_some_and(|p| p.owner != OWNER) {
        return Err(error(
            "FOREIGN_INSTALL",
            "다른 프로그램의 설치 기록은 덮어쓰지 않아요.",
        ));
    }
    if previous.as_ref().is_some_and(|p| {
        p.shims
            .iter()
            .any(|name| name != "aam" && !TOOLS.contains(&name.as_str()) && !LEGACY_TOOLS.contains(&name.as_str()))
            || p.native_binaries
                .keys()
                .any(|name| !TOOLS.contains(&name.as_str()) && !LEGACY_TOOLS.contains(&name.as_str()))
    }) {
        return Err(error(
            "INVALID_INSTALL_RECORD",
            "설치 기록의 명령 이름이 허용 목록에 없어요.",
        ));
    }
    let launcher =
        fs::canonicalize(std::env::current_exe().map_err(io_error)?).map_err(io_error)?;
    let mut native_binaries = previous
        .as_ref()
        .map(|p| p.native_binaries.clone())
        .unwrap_or_default();
    for name in LEGACY_TOOLS {
        native_binaries.remove(name);
    }
    let snapshot = call(paths, "status.read", json!({}))
        .ok()
        .and_then(|v| serde_json::from_value::<Snapshot>(v).ok());
    let mut search: Vec<PathBuf> =
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()).collect();
    search.extend([
        PathBuf::from("/opt/homebrew/bin"),
        PathBuf::from("/usr/local/bin"),
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default()
            .join(".local/bin"),
    ]);
    for tool in TOOLS {
        let previously_managed = previous
            .as_ref()
            .is_some_and(|p| p.shims.iter().any(|name| name == tool));
        let ready = snapshot.as_ref().is_some_and(|s| {
            s.tools.iter().any(|t| t.id == tool && t.installed)
                && s
                    .accounts
                    .iter()
                    .any(|a| a.tool == tool && a.enabled && a.can_launch)
        });
        // 기존 관리 도구는 일시적 인증 실패에도 원본으로 우회하지 않습니다.
        if !previously_managed && !ready {
            native_binaries.remove(tool);
            continue;
        }
        // 판이 없는 이전 기록과 그 기록을 읽은 이전 서비스의 관측은 해석된 버전별 경로라 다시 고정하지 않습니다.
        let current = previous
            .as_ref()
            .is_none_or(|p| p.version == INTEGRATION_VERSION);
        let observed = snapshot
            .as_ref()
            .filter(|_| current)
            .and_then(|s| s.tools.iter().find(|t| t.id == tool))
            .and_then(|t| t.binary_path.as_ref())
            .map(PathBuf::from);
        let prior = native_binaries
            .get(tool)
            .filter(|_| current)
            .map(PathBuf::from);
        let candidates = observed
            .into_iter()
            .chain(prior)
            .chain(search.iter().filter(|d| **d != bin).map(|d| d.join(tool)));
        // 진입 경로를 저장해야 공식 CLI 자체 업데이트 뒤에도 새 버전을 실행합니다.
        if let Some(found) = candidates
            .filter(|path| path.is_absolute() && !path.starts_with(&bin))
            .find(|path| {
                native_program(path).is_ok_and(|resolved| {
                    resolved.file_name().is_none_or(|name| name != "aam")
                        && previous.as_ref().is_none_or(|p| resolved != p.launcher_path)
                        && !resolved.starts_with(&bin)
                })
            })
        {
            native_binaries.insert(tool.to_owned(), found.to_string_lossy().into_owned());
        } else if !previously_managed {
            native_binaries.remove(tool);
        }
    }
    let mut shims = vec!["aam".to_owned()];
    shims.extend(native_binaries.keys().cloned());
    if let Some(previous) = &previous {
        for name in &previous.shims {
            if !shims.contains(name) && !LEGACY_TOOLS.contains(&name.as_str()) {
                shims.push(name.clone());
            }
        }
    }
    // 이전 판이 만든 legacy shim 링크는 앱 소유일 때만 지웁니다.
    if let Some(previous) = &previous {
        for name in previous.shims.iter().filter(|name| LEGACY_TOOLS.contains(&name.as_str())) {
            let target = bin.join(name);
            if fs::read_link(&target).is_ok_and(|link| link == launcher || link == previous.launcher_path) {
                fs::remove_file(&target).map_err(io_error)?;
            }
        }
    }
    // 모든 충돌을 먼저 검사하여 사용자 바이너리를 절대로 교체하지 않습니다.
    let mut original_links = BTreeMap::new();
    for name in &shims {
        let target = bin.join(name);
        match fs::symlink_metadata(&target) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                original_links.insert(name.clone(), None);
            }
            Ok(meta) if meta.file_type().is_symlink() => {
                let existing = fs::read_link(&target).map_err(io_error)?;
                let owned = existing == launcher
                    || previous
                        .as_ref()
                        .is_some_and(|p| p.shims.contains(name) && existing == p.launcher_path);
                if !owned {
                    return Err(error(
                        "SHIM_CONFLICT",
                        "명령 경로에 사용자 파일이 있어요. 기존 파일은 그대로 뒀어요.",
                    ));
                }
                original_links.insert(name.clone(), Some(existing));
            }
            _ => {
                return Err(error(
                    "SHIM_CONFLICT",
                    "명령 경로에 사용자 파일이 있어요. 기존 파일은 그대로 뒀어요.",
                ))
            }
        }
    }
    let record = Integration {
        owner: OWNER.into(),
        version: INTEGRATION_VERSION,
        launcher_path: launcher.clone(),
        native_binaries,
        shims: shims.clone(),
    };
    let mut changed = Vec::new();
    let result = (|| {
        for name in &shims {
            let target = bin.join(name);
            let original = &original_links[name];
            if original.as_ref() == Some(&launcher) {
                continue;
            }
            if fs::read_link(&target).ok().as_ref() != original.as_ref()
                || (original.is_none() && fs::symlink_metadata(&target).is_ok())
            {
                return Err(error(
                    "SHIM_CONFLICT",
                    "설치 중 명령 경로가 바뀌었어요. 사용자 파일은 그대로 뒀어요.",
                ));
            }
            if original.is_some() {
                fs::remove_file(&target).map_err(io_error)?;
            }
            changed.push(name.clone());
            symlink(&launcher, &target).map_err(io_error)?;
        }
        save(&record_path, &record)
    })();
    if let Err(failure) = result {
        // 이번 호출이 바꾼 링크만 복원하며, 중간에 사용자가 바꾼 파일은 건드리지 않습니다.
        for name in changed.iter().rev() {
            let target = bin.join(name);
            match fs::read_link(&target) {
                Ok(link) if link == launcher => {
                    fs::remove_file(&target).map_err(io_error)?;
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                _ => return Err(error("SHIM_ROLLBACK_CONFLICT", "설치 중 파일이 바뀌어 자동 복원을 멈췄어요. Ojak bin과 기존 설치 기록을 확인해 주세요.")),
            }
            if let Some(original) = &original_links[name] {
                symlink(original, &target).map_err(io_error)?;
            }
        }
        return Err(failure);
    }
    Ok(format!("등록된 도구의 Ojak 명령을 설치했어요. 이미 연결된 도구는 로그인 상태와 관계없이 그대로 둬요. 원래 CLI와 다른 앱 설정은 바꾸지 않았어요.\n지금 터미널에만 적용: export PATH={}:\"$PATH\"; hash -r\n계속 쓰려면 aam shell install (빼기: aam shell uninstall).\n진행 중인 작업은 그대로 두고, 새 터미널에서 command -v claude와 command -v codex를 확인해 주세요. 이미 연 zsh는 rehash 후 확인해 주세요. 다른 앱은 다시 켤 필요 없어요. 앱의 연결 안내에서 절대 경로를 확인해 주세요.\n모델을 비우면 도구의 기본 모델을 써요. 아직 확인되지 않은 도구는 새로 가로채지 않아요.", shell_quote(&bin)))
}

#[cfg(unix)]
pub fn integration_uninstall(paths: &Paths) -> Result<String, ApiError> {
    platform()?;
    let record_path = paths.home.join("integration.json");
    let Some(record): Option<Integration> = read_owned(&record_path)? else {
        return Ok(
            "설치 기록이 없어 명령을 지우지 않았어요. 셸 설정은 바꾸지 않았어요.".into(),
        );
    };
    if record.owner != OWNER {
        return Err(error(
            "FOREIGN_INSTALL",
            "이 앱 소유가 아닌 설치 기록은 지우지 않아요.",
        ));
    }
    let bin = paths.home.join("bin");
    for name in &record.shims {
        if name != "aam" && !TOOLS.contains(&name.as_str()) && !LEGACY_TOOLS.contains(&name.as_str()) {
            return Err(error(
                "INVALID_INSTALL_RECORD",
                "설치 기록의 명령 이름이 허용 목록에 없어요.",
            ));
        }
        let path = bin.join(name);
        if fs::symlink_metadata(&path).is_ok()
            && fs::read_link(&path).ok().as_ref() != Some(&record.launcher_path)
        {
            return Err(error(
                "SHIM_MODIFIED",
                "사용자가 바꾼 명령 파일이 있어 지우지 않았어요.",
            ));
        }
    }
    for name in &record.shims {
        match fs::remove_file(bin.join(name)) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(io_error(e)),
        }
    }
    fs::remove_file(record_path).map_err(io_error)?;
    let _ = fs::remove_dir(bin);
    Ok("이 앱이 만든 명령만 지웠어요. 원래 CLI, 로그인, 셸 설정은 그대로 뒀어요. 앱의 PATH 연결은 aam shell uninstall로 뺄 수 있어요.".into())
}

#[cfg(unix)]
fn launchctl(args: &[String]) -> Result<bool, ApiError> {
    let mut child = Command::new("/bin/launchctl")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| error("LAUNCHCTL_FAILED", "자동 실행 도구를 시작하지 못했어요."))?;
    let until = Instant::now() + Duration::from_secs(15);
    loop {
        match child.try_wait().map_err(io_error)? {
            Some(status) => return Ok(status.success()),
            None if Instant::now() >= until => {
                // 아직 회수하지 않은 직접 자식만 종료합니다. 서비스 프로세스에는 신호를 보내지 않습니다.
                let _ = child.kill();
                let _ = child.wait();
                return Err(error(
                    "LAUNCHCTL_TIMEOUT",
                    "자동 실행 도구가 응답하지 않아요. 서비스 상태를 다시 확인해 주세요.",
                ));
            }
            None => thread::sleep(Duration::from_millis(40)),
        }
    }
}
#[cfg(unix)]
fn domain() -> String {
    format!("gui/{}", unsafe { libc::geteuid() })
}
#[cfg(unix)]
fn target() -> String {
    format!("{}/{}", domain(), LABEL)
}
#[cfg(unix)]
fn xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}
#[cfg(unix)]
fn user_home() -> Result<PathBuf, ApiError> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .ok_or_else(|| error("HOME_MISSING", "사용자 홈의 절대 경로가 필요해요."))
}

/// 데몬이 CLI를 찾을 때 쓰는 고정 PATH. 설치 셸의 PATH는 넣지 않는다.
/// `adapters/src/process.rs` search_dirs의 사용자 bin·시스템 경로와 맞춘다. nvm 디렉터리는 데몬이 HOME에서 직접 더한다.
#[cfg(unix)]
fn service_path(home: &Path) -> Result<String, ApiError> {
    let mut dirs = vec![
        home.join(".local/bin"),
        home.join(".bun/bin"),
        home.join(".cargo/bin"),
    ];
    dirs.extend(["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin", "/bin", "/usr/sbin", "/sbin"].map(PathBuf::from));
    std::env::join_paths(dirs)
        .map(|path| path.to_string_lossy().into_owned())
        .map_err(|_| error("INVALID_PATH", "서비스 경로를 만들지 못했어요."))
}

#[cfg(unix)]
pub fn service_install(paths: &Paths) -> Result<String, ApiError> {
    platform()?;
    private_dir(&paths.home)?;
    let launcher =
        fs::canonicalize(std::env::current_exe().map_err(io_error)?).map_err(io_error)?;
    let service = native_program(
        &launcher
            .parent()
            .ok_or_else(|| error("BINARY_MISSING", "설치 경로를 찾지 못했어요."))?
            .join("aam-service"),
    )?;
    let record_path = paths.home.join("service-install.json");
    let old: Option<ServiceInstall> = read_owned(&record_path)?;
    if old.as_ref().is_some_and(|r| r.owner != OWNER) {
        return Err(error(
            "FOREIGN_INSTALL",
            "다른 프로그램의 설치 기록은 덮어쓰지 않아요.",
        ));
    }
    let home = user_home()?;
    let agents = home.join("Library/LaunchAgents");
    fs::create_dir_all(&agents).map_err(io_error)?;
    let plist = agents.join(format!("{LABEL}.plist"));
    if fs::symlink_metadata(&plist).is_ok() {
        let existing = fs::read_to_string(&plist).map_err(io_error)?;
        if old
            .as_ref()
            .is_none_or(|r| r.plist_path != plist || r.plist_contents != existing)
            || fs::symlink_metadata(&plist)
                .map_err(io_error)?
                .file_type()
                .is_symlink()
        {
            return Err(error(
                "LAUNCH_AGENT_CONFLICT",
                "기존 자동 실행이 앱 기록과 달라요. 덮어쓰지 않았어요.",
            ));
        }
    }
    if launchctl(&["print".into(), target()])? {
        match old {
            Some(mut previous) if previous.binary_path == service => {
                call(paths, "service.resumeAdmission", json!({}))?;
                previous.uninstall_permit = None;
                save(&record_path, &previous)?;
                let mut message = String::from("같은 관리 서비스가 이미 실행 중이고, 새 배정을 다시 열었어요.");
                if crate::service_version::read_report(paths).is_ok_and(|report| report.mismatch) {
                    message.push_str(" 실행 중인 서비스 버전이 앱과 달라요. 쓰는 중인 세션이 없을 때 `aam service restart`로 다시 시작해 주세요.");
                }
                return Ok(message);
            }
            // 앱 이름이나 위치가 바뀌어 예전 서비스 실행 파일이 사라졌으면, 앱 소유 LaunchAgent를 새 위치로 옮깁니다.
            // 서비스 재시작과 같으며 실행 중인 세션과 lease 기록은 그대로 둡니다.
            Some(previous) if !previous.binary_path.exists() => {
                if !launchctl(&["bootout".into(), target()])? {
                    return Err(error("SERVICE_BOOTOUT_FAILED", "이전 위치의 자동 실행을 끄지 못했어요. 설치 파일은 그대로 뒀어요."));
                }
                let deadline = Instant::now() + Duration::from_secs(10);
                while Instant::now() < deadline && call(paths, "status.read", json!({})).is_ok() {
                    thread::sleep(Duration::from_millis(200));
                }
            }
            _ => return Err(error("SERVICE_ALREADY_LOADED", "이미 등록된 서비스가 있어요. 진행 중인 작업을 끝낸 뒤 기존 서비스를 지우고 다시 설치해 주세요.")),
        }
    }
    match call(paths, "status.read", json!({})) {
        Err(error) if error.code == "DAEMON_UNAVAILABLE" => {}
        _ => {
            return Err(error(
                "UNMANAGED_SERVICE",
                "자동 실행이 아닌 서비스가 이미 실행 중이거나 응답을 확인하지 못했어요. 직접 켠 터미널에서 끈 뒤 설치해 주세요.",
            ));
        }
    }
    let logs = paths.home.join("logs");
    private_dir(&logs)?;
    for name in ["service.stdout.log", "service.stderr.log"] {
        let log = logs.join(name);
        if fs::symlink_metadata(&log).is_ok_and(|m| {
            !m.is_file() || m.file_type().is_symlink() || m.uid() != unsafe { libc::geteuid() }
        }) {
            return Err(error(
                "UNSAFE_LOG_PATH",
                "로그 경로가 이 앱의 일반 파일이 아니에요.",
            ));
        }
        OpenOptions::new()
            .append(true)
            .create(true)
            .mode(0o600)
            .open(log)
            .map_err(io_error)?;
    }
    let path = std::ffi::OsString::from(service_path(&home)?);
    let contents = format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\"><dict>\n<key>Label</key><string>{LABEL}</string>\n<key>ProgramArguments</key><array><string>{}</string></array>\n<key>RunAtLoad</key><true/><key>KeepAlive</key><true/>\n<key>ProcessType</key><string>Background</string>\n<key>EnvironmentVariables</key><dict><key>HOME</key><string>{}</string><key>AAM_HOME</key><string>{}</string><key>PATH</key><string>{}</string></dict>\n<key>StandardOutPath</key><string>{}</string>\n<key>StandardErrorPath</key><string>{}</string>\n</dict></plist>\n", xml(&service.to_string_lossy()), xml(&home.to_string_lossy()), xml(&paths.home.to_string_lossy()), xml(&path.to_string_lossy()), xml(&logs.join("service.stdout.log").to_string_lossy()), xml(&logs.join("service.stderr.log").to_string_lossy()));
    let record = ServiceInstall {
        owner: OWNER.into(),
        binary_path: service,
        plist_path: plist.clone(),
        plist_contents: contents.clone(),
        uninstall_permit: None,
    };
    save(&record_path, &record)?;
    atomic_write(&plist, contents.as_bytes())?;
    if !launchctl(&[
        "bootstrap".into(),
        domain(),
        plist.to_string_lossy().into_owned(),
    ])? {
        return Err(error("SERVICE_BOOTSTRAP_FAILED", "로그인 시 자동 실행 등록에 실패했어요. 설치 기록은 남겼으니 aam service status로 확인하거나 다시 설치해 주세요."));
    }
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        if call(paths, "status.read", json!({})).is_ok() {
            call(paths, "service.resumeAdmission", json!({}))?;
            return Ok("로그인 시 자동 실행을 설치하고, 서비스 연결과 새 배정 재개를 확인했어요. 앱 창을 닫아도 서비스는 유지돼요.".into());
        }
        thread::sleep(Duration::from_millis(200));
    }
    Err(error("SERVICE_NOT_READY", "자동 실행은 등록했지만 서비스 연결을 확인하지 못했어요. aam service status와 앱 로그를 확인해 주세요."))
}

#[cfg(unix)]
pub fn service_status(paths: &Paths) -> Result<String, ApiError> {
    platform()?;
    let loaded = launchctl(&["print".into(), target()])?;
    let healthy = call(paths, "status.read", json!({})).is_ok();
    Ok(format!(
        "로그인 시 자동 실행: {}\n서비스 연결: {}",
        if loaded { "등록됨" } else { "없음" },
        if healthy { "정상" } else { "연결 안 됨" }
    ))
}

/// 앱이 등록한 LaunchAgent만 `launchctl kickstart -k`로 다시 시작한다. 새 배정 차단·lease 검사는 호출한 쪽
/// (`service_version::restart`)이 이미 했다. 앱이 설치한 기록과 plist가 그대로일 때만 하고, 실행 파일이 사라졌으면
/// 서비스를 건드리지 않는다.
#[cfg(unix)]
pub(crate) fn service_kickstart(paths: &Paths) -> Result<(), ApiError> {
    platform()?;
    let Some(record): Option<ServiceInstall> = read_owned(&paths.home.join("service-install.json"))? else {
        return Err(error(
            "UNMANAGED_SERVICE",
            "앱이 설치한 자동 실행 기록이 없어 서비스를 다시 시작하지 않았어요. `aam service install`로 먼저 등록해 주세요.",
        ));
    };
    let expected = user_home()?.join("Library/LaunchAgents").join(format!("{LABEL}.plist"));
    if record.owner != OWNER || record.plist_path != expected {
        return Err(error("FOREIGN_INSTALL", "앱이 설치한 자동 실행 기록이 아니라서 다시 시작하지 않았어요."));
    }
    if fs::symlink_metadata(&expected).map_err(io_error)?.file_type().is_symlink()
        || fs::read_to_string(&expected).map_err(io_error)? != record.plist_contents
    {
        return Err(error("LAUNCH_AGENT_MODIFIED", "사용자가 바꾼 자동 실행은 다시 시작하지 않아요."));
    }
    if !record.binary_path.exists() {
        return Err(error(
            "BINARY_MISSING",
            "등록된 서비스 파일이 없어요. 서비스는 그대로 뒀어요. `aam service install`로 이 앱의 서비스를 다시 등록해 주세요.",
        ));
    }
    if !launchctl(&["print".into(), target()])? {
        return Err(error("SERVICE_NOT_LOADED", "자동 실행이 등록돼 있지 않아요. `aam service install`로 먼저 등록해 주세요."));
    }
    if !launchctl(&["kickstart".into(), "-k".into(), target()])? {
        return Err(error("SERVICE_RESTART_FAILED", "서비스를 다시 시작하지 못했어요. 서비스는 그대로 뒀어요. 잠시 뒤 다시 시도해 주세요."));
    }
    Ok(())
}

#[cfg(unix)]
pub fn service_uninstall(paths: &Paths) -> Result<String, ApiError> {
    service_shutdown(paths, true)
}

#[cfg(unix)]
pub fn service_stop(paths: &Paths) -> Result<String, ApiError> {
    service_shutdown(paths, false)
}

#[cfg(unix)]
fn service_shutdown(paths: &Paths, remove_installation: bool) -> Result<String, ApiError> {
    platform()?;
    let record_path = paths.home.join("service-install.json");
    let Some(mut record): Option<ServiceInstall> = read_owned(&record_path)? else {
        if !remove_installation {
            return Err(error(
                "UNMANAGED_SERVICE",
                "앱이 설치한 자동 실행 기록이 없어 서비스를 안전하게 끌 수 없어요. 직접 켠 서비스는 그 터미널에서 종료해 주세요.",
            ));
        }
        return Ok(
            "앱 설치 기록이 없어요. 자동 실행과 사용자 파일은 바꾸지 않았어요.".into(),
        );
    };
    let expected = user_home()?
        .join("Library/LaunchAgents")
        .join(format!("{LABEL}.plist"));
    if record.owner != OWNER || record.plist_path != expected {
        return Err(error(
            "FOREIGN_INSTALL",
            "앱이 설치한 자동 실행 기록이 아니라서 지우지 않아요.",
        ));
    }
    let plist_exists = fs::symlink_metadata(&expected).is_ok();
    if plist_exists
        && (fs::symlink_metadata(&expected)
            .map_err(io_error)?
            .file_type()
            .is_symlink()
            || fs::read_to_string(&expected).map_err(io_error)? != record.plist_contents)
    {
        return Err(error(
            "LAUNCH_AGENT_MODIFIED",
            "사용자가 바꾼 자동 실행은 지우지 않아요.",
        ));
    }
    let loaded = launchctl(&["print".into(), target()])?;
    if loaded {
        let prepared = call(paths, "service.prepareUninstall", json!({}))?;
        let permit = prepared
            .get("permit")
            .and_then(|v| v.as_str())
            .filter(|v| !v.is_empty());
        if prepared.get("safe").and_then(|v| v.as_bool()) != Some(true) || permit.is_none() {
            return Err(error(
                "INVALID_UNINSTALL_PERMIT",
                "새 배정 차단과 사용 중 안전 검사를 확인하지 못해 서비스를 끄지 않았어요.",
            ));
        }
        record.uninstall_permit = permit.map(str::to_owned);
        if let Err(error) = save(&record_path, &record) {
            let _ = call(
                paths,
                "service.cancelUninstall",
                json!({"permit":record.uninstall_permit}),
            );
            return Err(error);
        }
        let stopped = launchctl(&["bootout".into(), target()]);
        if !matches!(stopped, Ok(true)) {
            let restored = call(
                paths,
                "service.cancelUninstall",
                json!({"permit":record.uninstall_permit}),
            );
            if restored.is_ok() {
                record.uninstall_permit = None;
                save(&record_path, &record)?;
            }
            return Err(error("SERVICE_BOOTOUT_FAILED", "자동 실행이 꺼졌는지 확인하지 못했어요. 설치 파일은 그대로 뒀어요. 배정이 멈춰 있으면 aam service install로 다시 열어 주세요."));
        }
        if launchctl(&["print".into(), target()])? {
            return Err(error(
                "SERVICE_STILL_LOADED",
                "서비스가 아직 등록돼 있어 설치 파일은 그대로 뒀어요.",
            ));
        }
    } else if call(paths, "status.read", json!({})).is_ok() {
        return Err(error(
            "UNMANAGED_SERVICE",
            "자동 실행이 아닌 방식으로 켜진 서비스는 이 명령으로 끄지 않아요.",
        ));
    } else if paths.database.exists() && record.uninstall_permit.is_none() {
        return Err(error("LEASE_STATE_UNKNOWN", "서비스는 꺼져 있지만 저장된 사용 중 상태를 확인하지 못해요. 서비스를 다시 켠 뒤 안전하게 지워 주세요."));
    }
    if !remove_installation {
        return Ok("새 배정과 사용 중·불확실 상태의 안전 조건을 확인한 뒤 서비스를 껐어요. 자동 실행 파일과 계정·프로필·로그는 그대로 뒀어요. aam service install로 다시 연결할 수 있어요.".into());
    }
    if plist_exists {
        fs::remove_file(expected).map_err(io_error)?;
    }
    fs::remove_file(record_path).map_err(io_error)?;
    Ok("새 배정과 사용 중·불확실 상태의 안전 조건을 확인한 뒤, 앱이 설치한 자동 실행만 지웠어요. 계정·프로필·로그는 그대로 뒀어요.".into())
}

/// 시스템 PATH가 사용자 PATH보다 앞이라 shim을 가릴 때 보여 줄 문장. 도구 이름만 넣는다.
pub(crate) fn path_shadow_warning(tools: &[String]) -> Option<String> {
    if tools.is_empty() {
        return None;
    }
    let names = tools.join(", ");
    Some(format!(
        "주의: Windows는 시스템 PATH를 사용자 PATH보다 먼저 찾습니다. 시스템 PATH에 원본 {names}이(가) 있어 그 도구는 shim 대신 원본이 실행됩니다. `aam run <도구>`를 쓰거나 시스템 PATH에서 해당 설치를 제거한 뒤 새 터미널을 여세요."
    ))
}

#[cfg(unix)]
pub(crate) fn shadowed_tools() -> Vec<String> {
    Vec::new()
}

#[cfg(windows)]
#[path = "install_windows.rs"]
mod windows_install;
#[cfg(windows)]
pub(crate) use windows_install::{effective_command, shadowing_system_tools as shadowed_tools, shell_configured, shim_installed};
#[cfg(windows)]
pub use windows_install::{
    confirm_removal_target, integration_install, integration_uninstall, installer, remove_app_autostart,
    service_install, service_restart, service_status, service_stop, service_uninstall, shell_install,
    shell_uninstall,
};

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn registered_native_resolves_entry_link_and_rejects_shim_or_relative_paths() {
        let dir = std::env::temp_dir().join(format!("aam-registered-{}", aam_protocol::new_id()));
        fs::create_dir_all(dir.join("bin")).unwrap();
        let dir = fs::canonicalize(&dir).unwrap();
        let target = dir.join("1.0.1");
        fs::write(&target, b"\x7fELF").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).unwrap();
        let entry = dir.join("claude");
        symlink(&target, &entry).unwrap();
        symlink(&target, dir.join("bin/claude")).unwrap();
        let paths = Paths {
            home: dir.clone(),
            socket: dir.join("socket"),
            database: dir.join("unused.sqlite3"),
            profiles: dir.join("profiles"),
        };
        let register = |binary: &Path| {
            let record = Integration {
                owner: OWNER.into(),
                version: INTEGRATION_VERSION,
                launcher_path: PathBuf::from("/nonexistent/aam"),
                native_binaries: BTreeMap::from([("claude".into(), binary.to_string_lossy().into_owned())]),
                shims: vec!["aam".into(), "claude".into()],
            };
            save(&dir.join("integration.json"), &record).unwrap();
            registered_native(&paths, "claude")
        };
        assert_eq!(register(&entry).unwrap(), target);
        assert_eq!(register(&dir.join("bin/claude")).unwrap_err().code, "RECURSIVE_BINARY");
        assert_eq!(register(Path::new("claude")).unwrap_err().code, "INVALID_INSTALL_RECORD");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn service_path_is_a_fixed_safe_list() {
        let home = Path::new("/Users/example");
        let path = service_path(home).unwrap();
        assert_eq!(
            path,
            "/Users/example/.local/bin:/Users/example/.bun/bin:/Users/example/.cargo/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin"
        );
        assert!(!path.contains("node_modules"));
        assert!(!path.split(':').any(|entry| entry.contains("nvm")));
    }

}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;

    /// 관리자 그룹 사용자의 SSH·설치 실행은 승격 토큰이라 새 파일의 기본 소유자가 Administrators가 된다.
    /// 기록을 쓸 때 소유자를 현재 사용자로 명시하지 않으면 다음 일반 권한 실행에서 UNSAFE_PATH가 난다.
    #[test]
    fn saved_records_are_owned_by_the_user_even_from_an_elevated_writer() {
        let dir = std::env::temp_dir().join(format!("aam-owner-{}", aam_protocol::new_id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("record.json");
        save(&path, &serde_json::json!({ "owner": "test" })).unwrap();
        // 승격 실행이어도 소유자가 "정확히" 사용자 SID여야 한다. owned_by_current_user는 TOKEN_OWNER(Administrators)도
        // 인정하므로 이 회귀에는 쓰지 않는다. PowerShell 셸아웃은 CI 러너에서 빈 출력을 내 신뢰할 수 없다.
        let sid = aam_protocol::winutil::current_user_sid().unwrap();
        assert_eq!(aam_protocol::winutil::file_owner_sid(&path).unwrap(), sid);
        assert!(read_owned::<serde_json::Value>(&path).unwrap().is_some());
        fs::remove_dir_all(&dir).unwrap();
    }
}

#[cfg(test)]
mod launcher_ownership_tests {
    use super::*;

    #[test]
    fn maintenance_requires_the_current_owned_installation() {
        let home = std::env::temp_dir().join(format!("aam-current-install-{}", aam_protocol::new_id()));
        private_dir(&home).unwrap();
        let paths = Paths { socket: home.join("socket"), database: home.join("unused.sqlite3"), profiles: home.join("profiles"), home: home.clone() };
        assert!(!is_current_launcher(&paths).unwrap());
        let mut record = Integration {
            owner: OWNER.into(), version: INTEGRATION_VERSION,
            launcher_path: std::env::current_exe().unwrap(),
            native_binaries: BTreeMap::new(), shims: vec![],
        };
        save(&home.join("integration.json"), &record).unwrap();
        assert!(is_current_launcher(&paths).unwrap());
        save(&home.join("service-install.json"), &json!({"owner": OWNER, "binaryPath": home.join("another-install/aam-service.exe")})).unwrap();
        assert!(!is_current_launcher(&paths).unwrap());
        fs::remove_file(home.join("service-install.json")).unwrap();
        record.owner = "not-ojak".into();
        save(&home.join("integration.json"), &record).unwrap();
        assert_eq!(is_current_launcher(&paths).unwrap_err().code, "FOREIGN_INSTALL");
        fs::remove_dir_all(home).unwrap();
    }
}
