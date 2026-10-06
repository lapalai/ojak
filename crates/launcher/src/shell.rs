use super::{
    error, io_error, platform, private_dir, read_owned, save, shell_quote, Integration, OWNER,
};
use aam_protocol::{ApiError, Paths};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};

const BEGIN: &str = "# >>> ai-account-manager managed PATH >>>";
const END: &str = "# <<< ai-account-manager managed PATH <<<";

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ShellInstall {
    owner: String,
    target: PathBuf,
    block: String,
    original_existed: bool,
}

struct ConfigFile {
    bytes: Vec<u8>,
    mode: u32,
}

fn read_config(path: &Path) -> Result<Option<ConfigFile>, ApiError> {
    let mut file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if fs::symlink_metadata(path).is_ok() {
                return Err(error(
                    "UNSAFE_SHELL_PATH",
                    "shell 설정의 심볼릭 링크는 변경하지 않습니다.",
                ));
            }
            return Ok(None);
        }
        Err(_) => {
            return Err(error(
                "UNSAFE_SHELL_PATH",
                "shell 설정을 안전하게 열 수 없습니다. 심볼릭 링크와 파일 권한을 확인하세요.",
            ))
        }
    };
    let metadata = file.metadata().map_err(io_error)?;
    if !metadata.is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.nlink() != 1
        || metadata.len() > 1024 * 1024
    {
        return Err(error("UNSAFE_SHELL_PATH", "shell 설정은 현재 사용자 소유의 일반 파일이어야 합니다. 링크 파일은 변경하지 않습니다."));
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).map_err(io_error)?;
    Ok(Some(ConfigFile {
        bytes,
        mode: metadata.permissions().mode() & 0o777,
    }))
}

fn zshrc() -> Result<PathBuf, ApiError> {
    let directory = std::env::var_os("ZDOTDIR")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .ok_or_else(|| error("HOME_MISSING", "HOME 또는 ZDOTDIR가 필요합니다."))?;
    if !directory.is_absolute() {
        return Err(error(
            "INVALID_SHELL_PATH",
            "HOME과 ZDOTDIR는 절대 경로여야 합니다.",
        ));
    }
    // 상위 디렉터리 링크는 실제 경로로 고정하고 .zshrc 링크 자체는 거부합니다.
    let directory = fs::canonicalize(directory).map_err(io_error)?;
    check_parent(&directory.join(".zshrc"))?;
    Ok(directory.join(".zshrc"))
}

fn check_parent(path: &Path) -> Result<(), ApiError> {
    let parent = path
        .parent()
        .ok_or_else(|| error("INVALID_SHELL_PATH", "shell 설정 경로가 올바르지 않습니다."))?;
    let metadata = fs::symlink_metadata(parent).map_err(io_error)?;
    if !path.is_absolute()
        || path.file_name().is_none_or(|name| name != ".zshrc")
        || !metadata.is_dir()
        || metadata.file_type().is_symlink()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.permissions().mode() & 0o022 != 0
        || fs::canonicalize(parent).map_err(io_error)? != parent
    {
        return Err(error(
            "UNSAFE_SHELL_PATH",
            "shell 설정 디렉터리의 소유권 또는 경로가 변경되어 작업하지 않았습니다.",
        ));
    }
    Ok(())
}

fn unchanged(path: &Path, expected: Option<&[u8]>) -> Result<(), ApiError> {
    let current = read_config(path)?;
    if current.as_ref().map(|file| file.bytes.as_slice()) != expected {
        return Err(error(
            "SHELL_MODIFIED",
            "작업 중 shell 설정이 변경되어 보존했습니다. 다시 확인해 주세요.",
        ));
    }
    Ok(())
}

fn replace_config(
    path: &Path,
    expected: Option<&[u8]>,
    bytes: &[u8],
    mode: u32,
) -> Result<(), ApiError> {
    check_parent(path)?;
    let temp = path.with_extension(format!("aam-{}.tmp", aam_protocol::new_id()));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp)
            .map_err(io_error)?;
        file.write_all(bytes).map_err(io_error)?;
        file.set_permissions(fs::Permissions::from_mode(mode))
            .map_err(io_error)?;
        file.sync_all().map_err(io_error)?;
        unchanged(path, expected)?;
        fs::rename(&temp, path).map_err(io_error)
    })();
    if result.is_err() {
        let _ = fs::remove_file(temp);
    }
    result
}

fn occurrences(bytes: &[u8], needle: &[u8]) -> usize {
    bytes
        .windows(needle.len())
        .filter(|window| *window == needle)
        .count()
}

fn block_range(bytes: &[u8], block: &str) -> Result<std::ops::Range<usize>, ApiError> {
    if block.is_empty()
        || occurrences(bytes, BEGIN.as_bytes()) != 1
        || occurrences(bytes, END.as_bytes()) != 1
    {
        return Err(error(
            "SHELL_BLOCK_MODIFIED",
            "앱의 shell 블록이 삭제·중복·변경되어 설정을 보존했습니다.",
        ));
    }
    let start = bytes
        .windows(block.len())
        .position(|window| window == block.as_bytes())
        .ok_or_else(|| {
            error(
                "SHELL_BLOCK_MODIFIED",
                "사용자가 변경한 shell 블록은 덮어쓰거나 제거하지 않습니다.",
            )
        })?;
    if start != 0
        && bytes[start - 1] != b'\n'
        && !block.starts_with('\n')
        && !block.starts_with("\r\n")
    {
        return Err(error(
            "SHELL_BLOCK_MODIFIED",
            "앱 shell 블록의 시작 줄이 변경되어 설정을 보존했습니다.",
        ));
    }
    Ok(start..start + block.len())
}

fn validate_record(record: &ShellInstall) -> Result<(), ApiError> {
    if record.owner != OWNER
        || occurrences(record.block.as_bytes(), BEGIN.as_bytes()) != 1
        || occurrences(record.block.as_bytes(), END.as_bytes()) != 1
    {
        return Err(error(
            "INVALID_INSTALL_RECORD",
            "앱의 shell 설치 기록이 올바르지 않아 설정을 보존했습니다.",
        ));
    }
    check_parent(&record.target)
}

fn backup_path(paths: &Paths) -> PathBuf {
    paths.home.join("shell-zshrc.backup")
}

pub(crate) fn shell_configured(paths: &Paths) -> Result<bool, ApiError> {
    let Some(record): Option<ShellInstall> =
        read_owned(&paths.home.join("shell-integration.json"))?
    else {
        return Ok(false);
    };
    validate_record(&record)?;
    if record.target != zshrc()? {
        return Ok(false);
    }
    let Some(config) = read_config(&record.target)? else {
        return Ok(false);
    };
    block_range(&config.bytes, &record.block)?;
    Ok(true)
}

pub fn shell_install(paths: &Paths) -> Result<String, ApiError> {
    platform()?;
    private_dir(&paths.home)?;
    let target = zshrc()?;
    let record_path = paths.home.join("shell-integration.json");
    let previous: Option<ShellInstall> = read_owned(&record_path)?;
    if let Some(record) = previous {
        validate_record(&record)?;
        if record.target != target {
            return Err(error(
                "SHELL_TARGET_CHANGED",
                "기존 ZDOTDIR의 연결을 shell uninstall로 제거한 뒤 새 위치에 설치하세요.",
            ));
        }
        let current = read_config(&target)?;
        let bytes = current
            .as_ref()
            .map(|file| file.bytes.as_slice())
            .unwrap_or_default();
        block_range(bytes, &record.block)?;
        return Ok("zsh 연결이 이미 설치되어 있습니다. 기존 설정을 변경하지 않았습니다.".into());
    }
    let integration: Integration =
        read_owned(&paths.home.join("integration.json"))?.ok_or_else(|| {
            error(
                "INTEGRATION_REQUIRED",
                "먼저 aam integration install로 관리 bin을 설치하세요.",
            )
        })?;
    let bin = paths.home.join("bin");
    if integration.owner != OWNER
        || !integration.shims.iter().any(|name| name == "aam")
        || fs::read_link(bin.join("aam")).ok().as_ref() != Some(&integration.launcher_path)
        || !integration.launcher_path.is_file()
    {
        return Err(error(
            "INTEGRATION_REQUIRED",
            "관리 bin의 설치 상태가 올바르지 않습니다. integration install을 먼저 실행하세요.",
        ));
    }
    let bin_text = bin
        .to_str()
        .ok_or_else(|| error("INVALID_SHELL_PATH", "관리 bin 경로는 UTF-8이어야 합니다."))?;
    if bin_text.contains(['\n', '\r', ':']) {
        return Err(error(
            "INVALID_SHELL_PATH",
            "관리 bin 경로에는 줄바꿈이나 콜론을 사용할 수 없습니다.",
        ));
    }
    let current = read_config(&target)?;
    let bytes = current
        .as_ref()
        .map(|file| file.bytes.as_slice())
        .unwrap_or_default();
    if occurrences(bytes, BEGIN.as_bytes()) != 0 || occurrences(bytes, END.as_bytes()) != 0 {
        return Err(error(
            "SHELL_BLOCK_CONFLICT",
            "설치 기록이 없는 앱 shell 블록이 있어 기존 설정을 보존했습니다.",
        ));
    }
    let newline = if bytes.windows(2).any(|pair| pair == b"\r\n") {
        "\r\n"
    } else {
        "\n"
    };
    let prefix = if bytes.is_empty() || bytes.ends_with(b"\n") {
        ""
    } else {
        newline
    };
    let quoted = shell_quote(&bin);
    let block = format!("{prefix}{BEGIN}{newline}if [[ -d {quoted} && \"${{path[1]-}}\" != {quoted} ]]; then{newline}  path=({quoted} \"${{path[@]}}\"){newline}  export PATH{newline}fi{newline}{END}{newline}");
    // 원본 전체는 공개 설정 파일이 아닌 앱 전용 디렉터리에 0600으로 보관합니다.
    let mut backup = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(backup_path(paths))
        .map_err(io_error)?;
    backup.write_all(bytes).map_err(io_error)?;
    backup.sync_all().map_err(io_error)?;
    let record = ShellInstall {
        owner: OWNER.into(),
        target: target.clone(),
        block,
        original_existed: current.is_some(),
    };
    if let Err(e) = save(&record_path, &record) {
        let _ = fs::remove_file(backup_path(paths));
        return Err(e);
    }
    let mut installed = bytes.to_vec();
    installed.extend_from_slice(record.block.as_bytes());
    if let Err(e) = replace_config(
        &target,
        current.as_ref().map(|file| file.bytes.as_slice()),
        &installed,
        current.as_ref().map(|file| file.mode).unwrap_or(0o600),
    ) {
        let _ = fs::remove_file(&record_path);
        let _ = fs::remove_file(backup_path(paths));
        return Err(e);
    }
    Ok("zsh의 앱 전용 PATH 블록을 설치했습니다. 새 터미널을 열어 적용하세요. 제거: aam shell uninstall".into())
}

pub fn shell_uninstall(paths: &Paths) -> Result<String, ApiError> {
    platform()?;
    let record_path = paths.home.join("shell-integration.json");
    let Some(record): Option<ShellInstall> = read_owned(&record_path)? else {
        return Ok("shell 설치 기록이 없어 사용자 설정을 변경하지 않았습니다.".into());
    };
    validate_record(&record)?;
    let current = read_config(&record.target)?;
    let bytes = current
        .as_ref()
        .map(|file| file.bytes.as_slice())
        .unwrap_or_default();
    read_config(&backup_path(paths))?.ok_or_else(|| {
        error(
            "SHELL_BACKUP_MISSING",
            "shell 원본 백업이 없어 설정을 변경하지 않았습니다.",
        )
    })?;
    let range = block_range(bytes, &record.block)?;
    let mut restored = Vec::with_capacity(bytes.len() - range.len());
    restored.extend_from_slice(&bytes[..range.start]);
    restored.extend_from_slice(&bytes[range.end..]);
    if !record.original_existed && restored.is_empty() {
        unchanged(&record.target, Some(bytes))?;
        fs::remove_file(&record.target).map_err(io_error)?;
    } else {
        replace_config(
            &record.target,
            Some(bytes),
            &restored,
            current.as_ref().map(|file| file.mode).unwrap_or(0o600),
        )?;
    }
    fs::remove_file(record_path).map_err(io_error)?;
    fs::remove_file(backup_path(paths)).map_err(io_error)?;
    Ok("앱의 zsh PATH 블록만 제거했습니다. 나머지 설정과 alias는 보존했습니다. 새 터미널을 열어 적용하세요.".into())
}
