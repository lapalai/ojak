//! AAM이 소유하는 omp 확장의 설치·제거. 관측 확장과 계정 확장이 함께 쓴다.
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    env,
    error::Error,
    fs::{self, DirBuilder, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};
#[cfg(unix)]
use std::{fs::File, os::unix::fs::{DirBuilderExt, MetadataExt}};

/// AAM이 설치·제거하는 omp 확장. 소유 기록과 해시로 사용자 파일을 덮어쓰지 않는다.
pub(crate) struct Extension {
    pub source: &'static [u8],
    pub owner: &'static str,
    /// `<agent>/extensions/` 아래 디렉터리 이름.
    pub directory: &'static str,
}

const ENTRY: &str = "index.js";
const RECEIPT: &str = ".aam-owner.json";
const FILE_LIMIT: u64 = 65_536;

pub(crate) type Result<T, E = Box<dyn Error>> = std::result::Result<T, E>;

pub(crate) fn failure(message: &str) -> Box<dyn Error> {
    io::Error::other(message).into()
}

fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn installed_source(extension: &Extension) -> Result<Vec<u8>> {
    let home = aam_protocol::Paths::discover()?.home;
    if !home.is_absolute() {
        return Err(failure("AAM_HOME은 절대 경로여야 합니다."));
    }
    let home = home
        .to_str()
        .ok_or_else(|| failure("AAM_HOME은 UTF-8 경로여야 합니다."))?;
    let source = std::str::from_utf8(extension.source)?;
    let configured = format!(
        "const INSTALLED_AAM_HOME = {};",
        serde_json::to_string(home)?
    );
    // The shim and desktop sibling must fingerprint the same helper executable.
    let writer = format!("const INSTALLED_AAM_WRITER = {};", serde_json::to_string(&env::current_exe()?.canonicalize()?)?);
    Ok(source
        .replacen("const INSTALLED_AAM_HOME = undefined;", &configured, 1)
        .replacen("const INSTALLED_AAM_WRITER = undefined;", &writer, 1)
        .into_bytes())
}

pub(crate) fn extension_directory(extension: &Extension) -> Result<PathBuf> {
    let home = aam_protocol::user_home()
        .ok_or_else(|| failure("사용자 홈 경로를 확인할 수 없습니다."))?;
    if !home.is_absolute() {
        return Err(failure("HOME은 절대 경로여야 합니다."));
    }
    let agent = match env::var_os("PI_CODING_AGENT_DIR").filter(|value| !value.is_empty()) {
        Some(value) => {
            let path = PathBuf::from(value);
            if let Ok(relative) = path.strip_prefix("~") {
                home.join(relative)
            } else {
                path
            }
        }
        None => home.join(".omp/agent"),
    };
    if !agent.is_absolute()
        || agent
            .components()
            .any(|part| matches!(part, std::path::Component::ParentDir))
    {
        return Err(failure(
            "PI_CODING_AGENT_DIR은 상위 이동 없는 절대 경로여야 합니다.",
        ));
    }
    Ok(agent.join("extensions").join(extension.directory))
}

pub(crate) fn reject_symlink_ancestors(path: &Path) -> Result<()> {
    for ancestor in path.ancestors() {
        match fs::symlink_metadata(ancestor) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                return Err(failure(
                    "확장 설치 경로에 심볼릭 링크 또는 디렉터리가 아닌 항목이 있습니다.",
                ));
            }
            #[cfg(windows)]
            Ok(_) if aam_protocol::winutil::is_reparse_point(ancestor)? => {
                return Err(failure("확장 설치 경로에 재분석 지점이 있습니다."));
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(_) => return Err(failure("확장 설치 경로의 권한을 확인할 수 없습니다.")),
        }
    }
    Ok(())
}

pub(crate) fn owned_directory(path: &Path, private: bool) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    #[cfg(unix)]
    let safe = metadata.uid() == unsafe { libc::geteuid() }
        && metadata.mode() & (if private { 0o077 } else { 0o022 }) == 0;
    #[cfg(windows)]
    let safe = {
        use std::os::windows::fs::OpenOptionsExt;
        let file = OpenOptions::new().read(true)
            .custom_flags(0x02000000 | 0x00200000).open(path)?;
        file.metadata()?.is_dir()
            && !aam_protocol::winutil::is_reparse_point(path)?
            && aam_protocol::winutil::handle_access_is_safe(&file, private)?
    };
    if !metadata.is_dir()
        || metadata.file_type().is_symlink()
        || !safe
    {
        return Err(failure(
            "소유자 또는 권한이 안전하지 않은 확장 디렉터리입니다.",
        ));
    }
    Ok(())
}

fn read_owned(path: &Path, limit: u64) -> Result<Vec<u8>> {
    let mut file = aam_protocol::secure::open_read_no_follow(path)?;
    let metadata = file.metadata()?;
    #[cfg(unix)]
    let safe = metadata.mode() & 0o077 == 0 && metadata.nlink() == 1;
    #[cfg(windows)]
    let safe = aam_protocol::winutil::single_link_regular_file(&file)?
        && aam_protocol::winutil::handle_access_is_safe(&file, true)?;
    if !metadata.is_file()
        || !aam_protocol::secure::file_owned_by_me(&file)?
        || !safe
        || metadata.len() > limit
    {
        return Err(failure(
            "확장 소유권 또는 파일 무결성을 확인할 수 없습니다.",
        ));
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    (&mut file).take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(failure("확장 파일 크기가 허용 범위를 초과했습니다."));
    }
    Ok(bytes)
}

pub(crate) struct Installed {
    source_hash: String,
    pub current: bool,
}

pub(crate) fn inspect(extension: &Extension, directory: &Path) -> Result<Option<Installed>> {
    reject_symlink_ancestors(directory)?;
    match fs::symlink_metadata(directory) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(failure("확장 설치 상태를 읽을 수 없습니다.")),
        Ok(_) => {}
    }
    owned_directory(directory, true)?;
    // 다른 파일은 이름만 확인합니다. 사용자가 추가한 파일은 읽거나 삭제하지 않습니다.
    let names: Vec<_> = fs::read_dir(directory)?
        .map(|entry| entry.map(|entry| entry.file_name()))
        .collect::<io::Result<_>>()?;
    if names.len() != 2
        || !names.iter().any(|name| name == ENTRY)
        || !names.iter().any(|name| name == RECEIPT)
    {
        return Err(failure(
            "같은 이름의 기존 확장 또는 수정된 파일이 있어 작업을 거부했습니다.",
        ));
    }
    let receipt: Value = serde_json::from_slice(&read_owned(&directory.join(RECEIPT), 4_096)?)?;
    if receipt.get("owner").and_then(Value::as_str) != Some(extension.owner)
        || receipt.get("version").and_then(Value::as_u64) != Some(1)
    {
        return Err(failure(
            "Ojak 소유 확장이 아니므로 변경하지 않습니다.",
        ));
    }
    let source_hash = hash(&read_owned(&directory.join(ENTRY), FILE_LIMIT)?);
    if receipt.get("sha256").and_then(Value::as_str) != Some(source_hash.as_str()) {
        return Err(failure(
            "설치 후 변경된 확장이므로 덮어쓰거나 제거하지 않습니다.",
        ));
    }
    Ok(Some(Installed {
        current: source_hash == hash(&installed_source(extension)?),
        source_hash,
    }))
}

fn write_new(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    let mut file = aam_protocol::secure::private_options(&mut options).open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    // Windows: 승격 실행이 만든 파일의 기본 소유자는 Administrators다. 일반 권한 실행의 `read_owned`가
    // 남의 파일로 거부하지 않도록 현재 사용자 SID를 소유자로 명시한다. hard link는 같은 보안 설명자를 쓴다.
    #[cfg(windows)]
    aam_protocol::secure::restrict_file(path)?;
    Ok(())
}

pub(crate) fn install(extension: &Extension, directory: &Path) -> Result<bool> {
    if let Some(installed) = inspect(extension, directory)? {
        if installed.current {
            return Ok(false);
        }
        return Err(failure("이전 관리형 확장이 설치되어 있습니다. uninstall 후 install로 갱신하십시오. 실행 중인 OMP는 유지됩니다."));
    }
    let source = installed_source(extension)?;
    let receipt =
        serde_json::to_vec(&json!({ "owner": extension.owner, "version": 1, "sha256": hash(&source) }))?;
    let extensions = directory
        .parent()
        .ok_or_else(|| failure("확장 상위 경로가 없습니다."))?;
    reject_symlink_ancestors(extensions)?;
    let mut builder = DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    builder.mode(0o700);
    builder.create(extensions)?;
    reject_symlink_ancestors(extensions)?;
    owned_directory(extensions, false)?;
    // create_dir가 충돌 시 실패하므로 빈 기존 디렉터리도 대체하지 않습니다.
    #[cfg(unix)]
    DirBuilder::new().mode(0o700).create(directory)?;
    #[cfg(windows)]
    fs::create_dir(directory)?;
    #[cfg(windows)]
    aam_protocol::secure::restrict_dir(directory)?;
    let temporary = directory.join(".index.installing");
    let result = (|| -> Result<()> {
        write_new(&directory.join(RECEIPT), &receipt)?;
        write_new(&temporary, &source)?;
        // 완성된 entry만 공개합니다. hard_link는 목적지가 있으면 덮어쓰지 않습니다.
        fs::hard_link(&temporary, directory.join(ENTRY))?;
        fs::remove_file(&temporary)?;
        #[cfg(unix)]
        File::open(directory)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        // 실패 파일을 재귀 삭제하지 않습니다. 부분 설치는 status에서 충돌로 드러납니다.
        return Err(failure("확장 설치가 완료되지 않았습니다. 기존 파일은 보존했으며 부분 설치 경로를 확인하십시오."));
    }
    Ok(true)
}

pub(crate) fn uninstall(extension: &Extension, directory: &Path) -> Result<bool> {
    let Some(installed) = inspect(extension, directory)? else {
        return Ok(false);
    };
    // 검증한 소유 디렉터리를 같은 부모의 비검색 이름으로 격리한 뒤 다시 검증합니다.
    let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let parent = directory
        .parent()
        .ok_or_else(|| failure("확장 상위 경로가 없습니다."))?;
    let staging = parent.join(format!(
        ".{}-remove-{}-{nonce}",
        extension.directory,
        std::process::id()
    ));
    #[cfg(unix)]
    DirBuilder::new().mode(0o700).create(&staging)?;
    fs::rename(directory, &staging)?;
    let checked = inspect(extension, &staging);
    if !matches!(&checked, Ok(Some(value)) if value.source_hash == installed.source_hash) {
        // 검증 실패 파일은 삭제하지 않습니다. 새 파일과 충돌하면 되돌리기도 하지 않습니다.
        if fs::symlink_metadata(directory).is_err() {
            let _ = fs::rename(&staging, directory);
        }
        return Err(failure("확장 제거 중 파일이 변경되어 삭제하지 않았습니다."));
    }
    fs::remove_file(staging.join(ENTRY))?;
    fs::remove_file(staging.join(RECEIPT))?;
    fs::remove_dir(&staging)?;
    Ok(true)
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;

    /// 승격 실행(SSH·설치)이 확장 파일을 써도 다음 일반 권한 실행이 Ojak 소유 파일로 읽을 수 있어야 한다.
    #[test]
    fn extension_files_are_owned_by_the_user_even_from_an_elevated_writer() {
        let dir = std::env::temp_dir().join(format!("aam-extension-owner-{}", aam_protocol::new_id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(RECEIPT);
        write_new(&path, b"{}").unwrap();
        // 소유자가 정확히 사용자 SID여야 한다(TOKEN_OWNER 불인정). PowerShell 셸아웃은 CI 러너에서 빈 출력을 낸다.
        let sid = aam_protocol::winutil::current_user_sid().unwrap();
        assert_eq!(aam_protocol::winutil::file_owner_sid(&path).unwrap(), sid);
        assert_eq!(read_owned(&path, 4_096).unwrap(), b"{}");
        fs::remove_dir_all(&dir).unwrap();
    }
}

