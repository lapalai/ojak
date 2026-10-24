//! Windows 설치: shim(`AAM_HOME\bin\aam.cmd`·`claude.cmd`·`codex.cmd`), 사용자 PATH, 로그인 시 서비스 자동 실행.
//! `aam.cmd`는 launcher를 그대로 부르고, 도구 shim은 `--shim <tool>`로 부른다. 원본과 이름이 같은 서명 없는
//! `claude.exe` 복사본은 Defender가 위장 실행 파일로 보고 격리했다(`Behavior:Win32/Execution.A!ml`).
//! 사용자 파일은 교체하지 않는다. 앱이 기록한 이름·값만 만들고 지운다.
use super::{
    atomic_write, call, error, io_error, native_program, private_dir, read_owned, save, ApiError, Integration,
    Paths, Snapshot, INTEGRATION_VERSION, OWNER, TOOLS,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    collections::BTreeMap,
    fs,
    ffi::OsString,
    os::windows::{ffi::OsStringExt, fs::{MetadataExt, OpenOptionsExt}, io::{AsRawHandle, FromRawHandle, IntoRawHandle, OwnedHandle}},
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::{ERROR_FILE_NOT_FOUND, FILETIME, WAIT_OBJECT_0, WAIT_TIMEOUT},
    Storage::FileSystem::{FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ},
    System::{
        Registry::{
            RegCloseKey, RegCreateKeyExW, RegDeleteValueW, RegGetValueW, RegOpenKeyExW, RegSetValueExW, HKEY,
            HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_READ, KEY_WRITE, REG_EXPAND_SZ, REG_OPTION_NON_VOLATILE,
            REG_SZ, RRF_NOEXPAND, RRF_RT_REG_EXPAND_SZ, RRF_RT_REG_SZ,
        },
        Threading::{
            GetProcessTimes, OpenProcess, QueryFullProcessImageNameW, TerminateProcess, WaitForSingleObject,
            PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE,
        },
    },
    UI::WindowsAndMessaging::{
        SendMessageTimeoutW, HWND_BROADCAST, SMTO_ABORTIFHUNG, WM_SETTINGCHANGE,
    },
};

const ENVIRONMENT_KEY: &str = "Environment";
const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
/// 백그라운드 서비스의 Run 값. 앱의 로그인 자동 실행(tauri-plugin-autostart)이 제품 이름 `Ojak`을 쓰므로 겹치지 않게 둔다.
const RUN_VALUE: &str = "Ojak Service";
/// 이전 버전이 서비스에 쓰던 이름. 우리 기록과 값이 같을 때만 새 이름으로 옮긴다.
const LEGACY_RUN_VALUE: &str = "Ojak";
/// 앱 로그인 자동 실행. tauri-plugin-autostart가 `package_info().name`(제품 이름 `Ojak`)으로 Run 값을 만든다.
const APP_RUN_VALUE: &str = "Ojak";

#[path = "install_update_windows.rs"]
mod update;
#[path = "install_path_windows.rs"]
mod install_path;

pub fn installation_directory(path: &Path) -> Result<PathBuf, ApiError> {
    install_path::resolve(path)
}

pub fn write_installer_directory(path: &Path, output: &Path) -> Result<(), ApiError> {
    use std::{io::Write, os::windows::ffi::OsStrExt};
    let resolved = install_path::resolve_for_creation(path)?;
    let wide: Vec<u16> = resolved.as_os_str().encode_wide().collect();
    // NSIS와 레지스트리는 Win32 드라이브 경로를 사용한다. 콘솔 코드 페이지로 변환하지 않는다.
    let wide = wide.strip_prefix(&[92, 92, 63, 92]).unwrap_or(&wide);
    if wide.len() > 1022 || wide.len() < 3 || wide[1] != 58 || wide[2] != 92
        || wide.iter().any(|ch| matches!(*ch, 0 | 10 | 13))
    {
        return Err(error("INVALID_INSTALL_PATH", "설치 프로그램이 안전하게 처리할 수 없는 경로입니다."));
    }
    let bytes: Vec<u8> = wide.iter().flat_map(|ch| ch.to_le_bytes()).collect();
    let mut file = fs::OpenOptions::new().write(true).create_new(true).open(output).map_err(io_error)?;
    file.write_all(&bytes).map_err(io_error)?;
    file.sync_all().map_err(io_error)
}

pub fn installer(paths: &Paths, action: &str, directory: &Path) -> Result<(), ApiError> {
    update::run(paths, action, directory)
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

struct Key(HKEY);
impl Drop for Key {
    fn drop(&mut self) {
        unsafe {
            RegCloseKey(self.0);
        }
    }
}

fn open_key(path: &str, write: bool) -> Result<Key, ApiError> {
    let mut key: HKEY = std::ptr::null_mut();
    let status = unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            wide(path).as_ptr(),
            0,
            std::ptr::null(),
            REG_OPTION_NON_VOLATILE,
            if write { KEY_READ | KEY_WRITE } else { KEY_READ },
            std::ptr::null(),
            &mut key,
            std::ptr::null_mut(),
        )
    };
    if status != 0 {
        return Err(error("REGISTRY_ERROR", "사용자 레지스트리를 열지 못했습니다."));
    }
    Ok(Key(key))
}

/// 문자열 값을 확장하지 않고 읽는다. 없으면 None. 형식(REG_SZ/REG_EXPAND_SZ)도 함께 돌려준다.
fn read_value(key: &Key, name: &str) -> Result<Option<(String, u32)>, ApiError> {
    let name = wide(name);
    let flags = RRF_RT_REG_SZ | RRF_RT_REG_EXPAND_SZ | RRF_NOEXPAND;
    let mut kind = 0u32;
    let mut size = 0u32;
    let status = unsafe {
        RegGetValueW(key.0, std::ptr::null(), name.as_ptr(), flags, &mut kind, std::ptr::null_mut(), &mut size)
    };
    if status == ERROR_FILE_NOT_FOUND {
        return Ok(None);
    }
    if status != 0 || size as usize > 64 * 1024 {
        return Err(error("REGISTRY_ERROR", "사용자 레지스트리 값을 읽지 못했습니다."));
    }
    let mut buffer = vec![0u16; (size as usize).div_ceil(2)];
    let status = unsafe {
        RegGetValueW(key.0, std::ptr::null(), name.as_ptr(), flags, &mut kind, buffer.as_mut_ptr().cast(), &mut size)
    };
    if status != 0 {
        return Err(error("REGISTRY_ERROR", "사용자 레지스트리 값을 읽지 못했습니다."));
    }
    let len = buffer.iter().position(|c| *c == 0).unwrap_or(buffer.len());
    Ok(Some((String::from_utf16_lossy(&buffer[..len]), kind)))
}

fn write_value(key: &Key, name: &str, value: &str, kind: u32) -> Result<(), ApiError> {
    let data = wide(value);
    let status = unsafe {
        RegSetValueExW(key.0, wide(name).as_ptr(), 0, kind, data.as_ptr().cast(), (data.len() * 2) as u32)
    };
    if status != 0 {
        return Err(error("REGISTRY_ERROR", "사용자 레지스트리 값을 쓰지 못했습니다."));
    }
    Ok(())
}

fn delete_value(key: &Key, name: &str) -> Result<(), ApiError> {
    let status = unsafe { RegDeleteValueW(key.0, wide(name).as_ptr()) };
    if status != 0 && status != ERROR_FILE_NOT_FOUND {
        return Err(error("REGISTRY_ERROR", "사용자 레지스트리 값을 지우지 못했습니다."));
    }
    Ok(())
}

/// 새 터미널이 바뀐 환경 변수를 읽도록 알린다. 응답하지 않는 창은 기다리지 않는다.
fn broadcast_environment() {
    let area = wide(ENVIRONMENT_KEY);
    let mut result = 0usize;
    unsafe {
        SendMessageTimeoutW(HWND_BROADCAST, WM_SETTINGCHANGE, 0, area.as_ptr() as isize, SMTO_ABORTIFHUNG, 5000, &mut result);
    }
}

fn same_dir(a: &str, b: &Path) -> bool {
    let normalize = |text: &str| text.trim().trim_end_matches('\\').to_ascii_lowercase();
    normalize(a) == normalize(&b.to_string_lossy())
}

// ---------------------------------------------------------------- shim

fn bin(paths: &Paths) -> PathBuf {
    paths.home.join("bin")
}

fn launcher() -> Result<PathBuf, ApiError> {
    fs::canonicalize(std::env::current_exe().map_err(io_error)?).map_err(io_error)
}

fn shim_file(bin: &Path, tool: &str) -> PathBuf { bin.join(format!("{tool}.cmd")) }
/// 이전 버전의 launcher 복사본 shim. 새 shim을 놓을 때 지운다(같은 폴더에서는 `.exe`가 `.cmd`보다 먼저 실행된다).
fn legacy_shim(bin: &Path, tool: &str) -> PathBuf { bin.join(format!("{tool}.exe")) }

/// cmd는 배치 파일을 OEM 코드 페이지로 읽는다. 사용자 폴더 이름이 한글이어도 깨지지 않도록 알려진 폴더는
/// 환경 변수로 적고, 나머지가 ASCII가 아니면 만들지 않는다. `%`는 배치에서 변수로 해석되지 않게 겹친다.
fn batch_path(path: &Path) -> Result<String, ApiError> {
    // canonicalize는 `\\?\C:\...`를 돌려준다. cmd는 이 형식을 실행하지 못한다.
    let raw = path.to_string_lossy();
    let text = match raw.strip_prefix(r"\\?\") {
        Some(rest) if rest.as_bytes().get(1) == Some(&b':') => rest.to_owned(),
        Some(_) => return Err(error("SHIM_PATH_UNSUPPORTED", "Ojak 설치 경로를 배치 파일로 적을 수 없어 shim을 만들지 않았습니다.")),
        None => raw.into_owned(),
    };
    let mut prefix = String::new();
    let mut rest = text.as_str();
    for name in ["LOCALAPPDATA", "APPDATA", "USERPROFILE"] {
        let Some(value) = std::env::var_os(name).map(|value| value.to_string_lossy().trim_end_matches('\\').to_owned()) else { continue };
        if !value.is_empty() && rest.len() > value.len() && rest[..value.len()].eq_ignore_ascii_case(&value) && rest.as_bytes()[value.len()] == b'\\' {
            prefix = format!("%{name}%");
            rest = &text[value.len()..];
            break;
        }
    }
    if !rest.is_ascii() || rest.contains('"') {
        return Err(error("SHIM_PATH_UNSUPPORTED", "Ojak 설치 경로에 배치 파일로 적을 수 없는 문자가 있어 shim을 만들지 않았습니다."));
    }
    Ok(format!("{prefix}{}", rest.replace('%', "%%")))
}

/// `<tool>.cmd`를 새로 쓰고, 남아 있는 이전 `<tool>.exe` 복사본을 치운다. 실행 중인 exe는 지울 수 없지만
/// 이름은 바꿀 수 있으므로 옆으로 옮긴 뒤 지우고, 지우지 못한 파일은 다음 설치에서 정리한다.
/// `aam`은 Mac 심볼릭 링크처럼 launcher를 그대로 부른다. `--shim`은 claude·codex만 받는다.
fn place_shim(launcher: &Path, bin: &Path, tool: &str) -> Result<(), ApiError> {
    let body = shim_body(launcher, tool)?;
    let target = shim_file(bin, tool);
    let temp = target.with_extension(format!("{}.new", aam_protocol::new_id()));
    fs::write(&temp, body).map_err(io_error)?;
    fs::rename(&temp, &target).map_err(|e| {
        let _ = fs::remove_file(&temp);
        io_error(e)
    })?;
    remove_shim_file(&legacy_shim(bin, tool))
}

fn shim_body(launcher: &Path, tool: &str) -> Result<String, ApiError> {
    let path = batch_path(launcher)?;
    if tool == "aam" {
        Ok(format!("@\"{path}\" %*\r\n"))
    } else {
        Ok(format!("@\"{path}\" --shim {tool} %*\r\n"))
    }
}

fn remove_shim_file(target: &Path) -> Result<(), ApiError> {
    if target.exists() {
        let parked = target.with_extension(format!("{}.old", aam_protocol::new_id()));
        fs::rename(target, &parked).map_err(io_error)?;
        let _ = fs::remove_file(parked);
    }
    Ok(())
}

fn remove_shim(bin: &Path, tool: &str) -> Result<(), ApiError> {
    remove_shim_file(&shim_file(bin, tool))?;
    remove_shim_file(&legacy_shim(bin, tool))
}

fn sweep_parked(dir: &Path) {
    for entry in fs::read_dir(dir).into_iter().flatten().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.ends_with(".old") || name.ends_with(".new") {
            let _ = fs::remove_file(entry.path());
        }
    }
}

/// 서비스가 방금 시작됐으면 첫 계정 조회가 끝날 때까지 기다린다. 끝나기 전 상태로는 준비된 계정이 없어 보인다.
fn refreshed_snapshot(paths: &Paths) -> Result<Snapshot, ApiError> {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let snapshot: Snapshot = serde_json::from_value(call(paths, "status.read", json!({}))?)
            .map_err(|_| error("INVALID_SNAPSHOT", "서비스 상태를 해석하지 못했습니다."))?;
        if (!snapshot.refreshing && snapshot.last_refresh_at.is_some()) || Instant::now() >= deadline {
            return Ok(snapshot);
        }
        thread::sleep(Duration::from_millis(500));
    }
}

pub(crate) fn shim_installed(paths: &Paths, tool: &str) -> Result<bool, ApiError> {
    let Some(record): Option<Integration> = read_owned(&paths.home.join("integration.json"))? else {
        return Ok(false);
    };
    if record.owner != OWNER {
        return Err(error(
            "FOREIGN_INSTALL",
            "앱 소유가 아닌 연결 기록입니다. 기존 파일을 보존하고 설치 경로를 확인하세요.",
        ));
    }
    Ok(record.shims.iter().any(|name| name == tool)
        && fs::metadata(shim_file(&bin(paths), tool)).is_ok_and(|m| m.is_file()))
}

/// 계정이 준비된 도구의 원본 CLI 진입 경로를 기록하고, 그 도구의 shim을 만든다.
pub fn integration_install(paths: &Paths) -> Result<String, ApiError> {
    private_dir(&paths.home)?;
    let bin = bin(paths);
    private_dir(&bin)?;
    sweep_parked(&bin);
    let record_path = paths.home.join("integration.json");
    let previous: Option<Integration> = read_owned(&record_path)?;
    if previous.as_ref().is_some_and(|p| p.owner != OWNER) {
        return Err(error("FOREIGN_INSTALL", "다른 프로그램의 설치 기록을 덮어쓰지 않습니다."));
    }
    let launcher = launcher()?;
    let snapshot = refreshed_snapshot(paths)?;
    let mut native_binaries = BTreeMap::new();
    for tool in TOOLS {
        let ready = snapshot.accounts.iter().any(|a| a.tool == tool && a.enabled && a.can_launch);
        let Some(found) = snapshot
            .tools
            .iter()
            .find(|t| t.id == tool && t.installed)
            .and_then(|t| t.binary_path.as_ref())
            .map(PathBuf::from)
            .filter(|path| ready && path.is_absolute() && !path.starts_with(&bin))
        else {
            continue;
        };
        if native_program(&found).is_ok_and(|resolved| resolved != launcher && !resolved.starts_with(&bin)) {
            native_binaries.insert(tool.to_owned(), found.to_string_lossy().into_owned());
        }
    }
    let owned: Vec<String> = previous.as_ref().map(|p| p.shims.clone()).unwrap_or_default();
    // 모든 충돌을 먼저 검사한다. 앱이 만든 적 없는 파일은 교체하지 않는다.
    let mut planned = vec!["aam".to_owned()];
    planned.extend(native_binaries.keys().cloned());
    for tool in &planned {
        let occupied = shim_file(&bin, tool).exists() || legacy_shim(&bin, tool).exists();
        if occupied && !owned.iter().any(|name| name == tool) {
            return Err(error("SHIM_CONFLICT", "shim 경로에 사용자 파일이 있습니다. 기존 파일은 보존했습니다."));
        }
    }
    let mut shims = Vec::new();
    for tool in &planned {
        place_shim(&launcher, &bin, tool)?;
        shims.push(tool.clone());
    }
    // 이번에 등록하지 않은 도구의 앱 소유 shim은 지운다(원본으로 바로 가게). aam은 항상 남긴다.
    for tool in owned.iter().filter(|name| !shims.contains(name)) {
        remove_shim(&bin, tool)?;
    }
    let saved = save(
        &record_path,
        &Integration {
            owner: OWNER.into(),
            version: INTEGRATION_VERSION,
            launcher_path: launcher,
            native_binaries,
            shims: shims.clone(),
        },
    );
    if let Err(failure) = saved {
        if !owned.iter().any(|name| name == "aam") {
            let _ = remove_shim(&bin, "aam");
        }
        return Err(failure);
    }
    let tools: Vec<&str> = shims.iter().filter(|name| name.as_str() != "aam").map(String::as_str).collect();
    Ok(if tools.is_empty() {
        "관리 명령 aam을 PATH에 넣었습니다. 실행할 수 있는 계정이 준비된 원본 CLI가 없어 claude·codex shim은 만들지 않았습니다.".into()
    } else {
        format!(
            "shim을 만들었습니다: {}. PATH가 설정된 새 터미널에서 `{}`처럼 실행하면 관리 실행됩니다.",
            shims.join(", "),
            tools[0]
        )
    })
}

pub fn integration_uninstall(paths: &Paths) -> Result<String, ApiError> {
    let record_path = paths.home.join("integration.json");
    let Some(record): Option<Integration> = read_owned(&record_path)? else {
        return Ok("Windows에는 설치된 연결이 없습니다.".into());
    };
    if record.owner != OWNER {
        return Err(error("FOREIGN_INSTALL", "다른 프로그램의 설치 기록을 지우지 않습니다."));
    }
    let bin = bin(paths);
    for tool in &record.shims {
        // 실행 중인 shim은 지울 수 없으므로 이름을 바꿔 PATH에서 빼고, 다음 설치 때 정리한다.
        remove_shim(&bin, tool)?;
    }
    fs::remove_file(&record_path).map_err(io_error)?;
    Ok("shim과 원본 CLI 등록을 지웠습니다. 새 터미널부터 원래 CLI가 실행됩니다.".into())
}

// ---------------------------------------------------------------- PATH

pub(crate) fn shell_configured(paths: &Paths) -> Result<bool, ApiError> {
    let key = open_key(ENVIRONMENT_KEY, false)?;
    let bin = bin(paths);
    Ok(read_value(&key, "Path")?
        .is_some_and(|(value, _)| value.split(';').next().is_some_and(|first| same_dir(first, &bin))))
}

/// 사용자 PATH 맨 앞에 shim 폴더를 넣는다. 기존 항목과 값 형식(REG_EXPAND_SZ)은 보존한다.
pub fn shell_install(paths: &Paths) -> Result<String, ApiError> {
    private_dir(&paths.home)?;
    let bin = bin(paths);
    private_dir(&bin)?;
    let key = open_key(ENVIRONMENT_KEY, true)?;
    let (current, kind) = read_value(&key, "Path")?.unwrap_or((String::new(), REG_EXPAND_SZ));
    let rest: Vec<&str> = current
        .split(';')
        .filter(|entry| !entry.trim().is_empty() && !same_dir(entry, &bin))
        .collect();
    let mut entries = vec![bin.to_string_lossy().into_owned()];
    entries.extend(rest.iter().map(|entry| (*entry).to_owned()));
    let next = entries.join(";");
    if next == current {
        return Ok("사용자 PATH에 이미 shim 폴더가 맨 앞에 있습니다.".into());
    }
    write_value(&key, "Path", &next, if kind == REG_SZ { REG_SZ } else { REG_EXPAND_SZ })?;
    broadcast_environment();
    let mut message = String::from("사용자 PATH 맨 앞에 shim 폴더를 넣었습니다. 새 터미널부터 적용됩니다.");
    if let Some(warning) = super::path_shadow_warning(&shadowing_system_tools()) {
        message.push(' ');
        message.push_str(&warning);
    }
    Ok(message)
}

/// 시스템(HKLM) PATH의 원본 문자열. 읽지 못하면 None.
fn machine_path() -> Option<String> {
    let mut key: HKEY = std::ptr::null_mut();
    let path = wide(r"SYSTEM\CurrentControlSet\Control\Session Manager\Environment");
    if unsafe { RegOpenKeyExW(HKEY_LOCAL_MACHINE, path.as_ptr(), 0, KEY_READ, &mut key) } != 0 {
        return None;
    }
    let key = Key(key);
    read_value(&key, "Path").ok().flatten().map(|(value, _)| value)
}

fn path_dirs(value: &str) -> impl Iterator<Item = PathBuf> + '_ {
    value.split(';').map(|entry| entry.trim().trim_matches('"')).filter(|entry| !entry.is_empty()).map(|entry| PathBuf::from(expand(entry)))
}

/// 시스템(HKLM) PATH에 있는 원본 CLI. 시스템 PATH가 사용자 PATH보다 앞이라 shim을 가린다.
pub(crate) fn shadowing_system_tools() -> Vec<String> {
    let Some(value) = machine_path() else { return Vec::new() };
    let dirs: Vec<PathBuf> = path_dirs(&value).collect();
    TOOLS
        .iter()
        .filter(|tool| {
            dirs.iter().any(|dir| {
                ["exe", "cmd", "bat", "com"].iter().any(|ext| dir.join(format!("{tool}.{ext}")).is_file())
            })
        })
        .map(|tool| (*tool).to_owned())
        .collect()
}

/// 새 로그인 셸이 `tool`로 찾을 실행 파일. 저장된 시스템 PATH 다음 사용자 PATH 순서로, 폴더마다
/// PATHEXT 순서로 찾는다(`Get-Command -CommandType Application`의 첫 결과와 같은 규칙).
/// 이미 열린 터미널의 alias·함수는 보지 않는다.
pub(crate) fn effective_command(tool: &str) -> Option<PathBuf> {
    let user = open_key(ENVIRONMENT_KEY, false).ok().and_then(|key| read_value(&key, "Path").ok().flatten()).map(|(value, _)| value);
    let combined = format!("{};{}", machine_path().unwrap_or_default(), user.unwrap_or_default());
    let pathext = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into());
    let extensions: Vec<&str> = pathext.split(';').map(str::trim).filter(|ext| ext.starts_with('.')).collect();
    let found = path_dirs(&combined).find_map(|dir| {
        extensions.iter().map(|ext| dir.join(format!("{tool}{ext}"))).find(|candidate| candidate.is_file())
    });
    found
}

/// `%SystemRoot%` 같은 환경 변수를 펼친다. 실패하면 원문을 쓴다.
fn expand(text: &str) -> String {
    use windows_sys::Win32::System::Environment::ExpandEnvironmentStringsW;
    let source = wide(text);
    let mut buffer = vec![0u16; 4096];
    let len = unsafe { ExpandEnvironmentStringsW(source.as_ptr(), buffer.as_mut_ptr(), buffer.len() as u32) };
    if len == 0 || len as usize > buffer.len() {
        return text.to_owned();
    }
    String::from_utf16_lossy(&buffer[..len as usize - 1])
}

pub fn shell_uninstall(paths: &Paths) -> Result<String, ApiError> {
    let key = open_key(ENVIRONMENT_KEY, true)?;
    let Some((current, kind)) = read_value(&key, "Path")? else {
        return Ok("사용자 PATH가 없습니다. 바꾼 것이 없습니다.".into());
    };
    let bin = bin(paths);
    let kept: Vec<&str> = current.split(';').filter(|entry| !same_dir(entry, &bin)).collect();
    let next = kept.join(";");
    if next == current {
        return Ok("사용자 PATH에 shim 폴더가 없습니다. 바꾼 것이 없습니다.".into());
    }
    write_value(&key, "Path", &next, kind)?;
    broadcast_environment();
    Ok("사용자 PATH에서 shim 폴더만 뺐습니다. 새 터미널부터 적용됩니다.".into())
}

// ---------------------------------------------------------------- service

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ServiceRecord {
    owner: String,
    binary_path: PathBuf,
    run_value: String,
    #[serde(default)]
    uninstall_permit: Option<String>,
}

fn service_binary() -> Result<PathBuf, ApiError> {
    let path = launcher()?.with_file_name("aam-service.exe");
    if !fs::metadata(&path).is_ok_and(|m| m.is_file()) {
        return Err(error("BINARY_MISSING", "앱과 함께 설치된 aam-service.exe를 찾지 못했습니다."));
    }
    Ok(path)
}

fn running(paths: &Paths) -> bool {
    call(paths, "status.read", json!({})).is_ok()
}

/// 콘솔·작업 창과 분리해 서비스를 시작한다. 터미널이나 원격 세션이 닫혀도 함께 끝나지 않게 한다.
/// 새 서비스 실행 파일은 인자 없이 시작하면 감독 프로세스가 되어 서비스(작업자)를 자식으로 띄운다. 감독이 없던 이전 판의
/// 실행 파일은 같은 명령으로 서비스 자신이 된다. 그래서 인자를 넘기지 않아 복구(이전 판 되돌리기)도 같은 경로를 쓴다.
/// 돌려주는 객체는 방금 만든 프로세스(감독 프로세스일 수 있다)이며, 서비스는 `started_worker`로 pipe에서 따로 찾는다.
fn start_detached(binary: &Path, paths: &Paths) -> Result<ServiceProcess, ApiError> {
    use std::{os::windows::process::CommandExt, process::{Command, Stdio}};
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
    let base = DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW;
    aam_protocol::winutil::prevent_stdio_inheritance()
        .map_err(|_| error("SERVICE_START_FAILED", "준비 명령의 출력 연결을 분리하지 못해 서비스를 시작하지 않았습니다."))?;
    for flags in [base | CREATE_BREAKAWAY_FROM_JOB, base] {
        let spawned = Command::new(binary)
            // 서비스가 이 실행기와 같은 상태 폴더(와 그 이름의 pipe)를 쓰도록 명시한다. 기본 경로 추정에 맡기지 않는다.
            .env("AAM_HOME", &paths.home)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(flags)
            .spawn();
        if let Ok(child) = spawned {
            let pid = child.id();
            return Ok(ServiceProcess {
                handle: unsafe { OwnedHandle::from_raw_handle(child.into_raw_handle()) },
                pid,
                image: None,
            });
        }
    }
    Err(error("SERVICE_START_FAILED", "관리 서비스를 시작하지 못했습니다."))
}

pub fn service_install(paths: &Paths) -> Result<String, ApiError> {
    private_dir(&paths.home)?;
    let binary = service_binary()?;
    let record_path = paths.home.join("service-install.json");
    let previous: Option<ServiceRecord> = read_owned(&record_path)?;
    if previous.as_ref().is_some_and(|p| p.owner != OWNER) {
        return Err(error("FOREIGN_INSTALL", "다른 프로그램의 서비스 기록을 덮어쓰지 않습니다."));
    }
    let key = open_key(RUN_KEY, true)?;
    let run_value = format!("\"{}\"", binary.display());
    if let Some((legacy, _)) = read_value(&key, LEGACY_RUN_VALUE)? {
        if previous.as_ref().is_some_and(|p| p.run_value == legacy) {
            delete_value(&key, LEGACY_RUN_VALUE)?;
        }
    }
    if let Some((existing, _)) = read_value(&key, RUN_VALUE)? {
        let ours = previous.as_ref().is_some_and(|p| p.run_value == existing);
        if !ours && existing != run_value {
            return Err(error("FOREIGN_INSTALL", "같은 이름의 시작 프로그램 항목이 있습니다. 바꾸지 않았습니다."));
        }
    }
    save(
        &record_path,
        &ServiceRecord { owner: OWNER.into(), binary_path: binary.clone(), run_value: run_value.clone(), uninstall_permit: None },
    )?;
    write_value(&key, RUN_VALUE, &run_value, REG_SZ)?;
    // 서비스는 떠 있어도 감독 프로세스가 없으면(이전 판이 시작한 서비스) 감독 프로세스를 시작한다. 감독 프로세스는 이미 도는
    // 서비스를 재시작 없이 넘겨받아 지켜보므로 이 단계가 서비스를 끄지 않는다.
    if !running(paths) || !aam_service::supervisor::running(paths) {
        start_detached(&binary, paths)?;
        let deadline = Instant::now() + Duration::from_secs(15);
        while !(running(paths) && aam_service::supervisor::running(paths)) {
            if Instant::now() >= deadline {
                return Err(error("SERVICE_START_TIMEOUT", "로그인 시 자동 실행은 등록했지만 서비스와 감독 프로세스 시작을 확인하지 못했습니다."));
            }
            thread::sleep(Duration::from_millis(200));
        }
    }
    // The admission barrier is stored in the DB and survives a service restart.
    call(paths, "service.resumeAdmission", json!({}))?;
    Ok("관리 서비스를 로그인 시 자동 실행하도록 등록하고 시작했습니다.".into())
}

pub fn service_status(paths: &Paths) -> Result<String, ApiError> {
    let key = open_key(RUN_KEY, false)?;
    let registered = read_value(&key, RUN_VALUE)?.is_some();
    Ok(format!(
        "로그인 시 자동 실행: {} · 서비스: {} · 감독: {}",
        if registered { "등록됨" } else { "미등록" },
        if running(paths) { "실행 중" } else { "중지됨" },
        if aam_service::supervisor::running(paths) { "실행 중" } else { "없음" }
    ))
}

pub fn service_stop(paths: &Paths) -> Result<String, ApiError> {
    service_shutdown(paths, false)
}

pub fn service_uninstall(paths: &Paths) -> Result<String, ApiError> {
    service_shutdown(paths, true)
}


/// RPC 승인과 종료가 같은 프로세스 객체를 대상으로 하도록 핸들을 유지한다.
struct ServiceProcess {
    handle: OwnedHandle,
    pid: u32,
    image: Option<fs::File>,
}
impl ServiceProcess {
    fn connect(paths: &Paths, binary: &Path) -> Result<Option<Self>, ApiError> {
        let pid = match aam_protocol::service_pid(paths) {
            Ok(pid) => pid,
            Err(failure) if failure.raw_os_error() == Some(ERROR_FILE_NOT_FOUND as i32) => return Ok(None),
            Err(_) => return Err(error("UNMANAGED_SERVICE", "서비스 연결의 소유자를 확인하지 못해 복구 기록을 보존했습니다.")),
        };
        let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE, 0, pid) };
        if handle.is_null() { return Err(error("UNMANAGED_SERVICE", "서비스 프로세스를 확인하지 못해 중지하지 않았습니다.")); }
        let mut process = Self { handle: unsafe { OwnedHandle::from_raw_handle(handle as _) }, pid, image: None };
        if !aam_protocol::winutil::same_user_process(pid) || !process.alive() {
            return Err(error("UNMANAGED_SERVICE", "현재 사용자의 실행 중 서비스가 아니므로 중지하지 않았습니다."));
        }
        let parent = binary.parent().ok_or_else(|| error("FOREIGN_INSTALL", "서비스 설치 경로를 확인하지 못했습니다."))?;
        let binary = installation_directory(parent)?.join(binary.file_name().ok_or_else(|| error("FOREIGN_INSTALL", "서비스 실행 파일 이름이 없습니다."))?);
        // 동일 파일 객체를 유지하고 승인·종료가 끝날 때까지 쓰기와 이름 교체를 막는다.
        let file = fs::OpenOptions::new().read(true).share_mode(FILE_SHARE_READ)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT).open(&binary).map_err(io_error)?;
        let metadata = file.metadata().map_err(io_error)?;
        if !metadata.is_file() || metadata.file_attributes() & 0x400 != 0 {
            return Err(error("UPDATE_UNSAFE_FILE", "서비스 실행 파일의 종류를 확인하지 못했습니다."));
        }
        process.image = Some(file);
        let mut image = [0u16; 32768];
        let mut length = image.len() as u32;
        if unsafe { QueryFullProcessImageNameW(process.handle.as_raw_handle() as _, 0, image.as_mut_ptr(), &mut length) } == 0 {
            return Err(error("UNMANAGED_SERVICE", "서비스 실행 파일을 확인하지 못해 중지하지 않았습니다."));
        }
        let image = PathBuf::from(OsString::from_wide(&image[..length as usize]));
        if binary.file_name().and_then(|name| name.to_str()) != Some("aam-service.exe")
            || image.canonicalize().map_err(io_error)? != binary.canonicalize().map_err(io_error)?
        {
            return Err(error("FOREIGN_INSTALL", "다른 설치의 실행 중 서비스는 중지하지 않습니다."));
        }
        Ok(Some(process))
    }
    fn alive(&self) -> bool {
        unsafe { WaitForSingleObject(self.handle.as_raw_handle() as _, 0) == WAIT_TIMEOUT }
    }
    fn call(&self, paths: &Paths, method: &str, params: serde_json::Value) -> Result<serde_json::Value, ApiError> {
        if !self.alive() { return Err(error("SERVICE_PEER_CHANGED", "승인한 서비스가 종료되어 다른 프로세스에 요청하지 않았습니다.")); }
        aam_protocol::call_for_process(paths, method, params, self.pid)
    }
    fn prepare(&self, paths: &Paths) -> Result<String, ApiError> {
        let prepared = self.call(paths, "service.prepareUninstall", json!({}))?;
        let permit = prepared.get("permit").and_then(|value| value.as_str()).filter(|value| !value.is_empty());
        if prepared.get("safe").and_then(|value| value.as_bool()) != Some(true) || permit.is_none() {
            return Err(error("INVALID_UNINSTALL_PERMIT", "활성·불확실 lease의 안전 조건을 확인하지 못해 서비스를 중지하지 않았습니다."));
        }
        Ok(permit.unwrap().to_owned())
    }
    /// 종료 권한은 실제로 끝내야 하는 이 순간에만, 같은 프로세스 객체임을 다시 확인한 뒤 얻는다. 연결 핸들은 조회·대기 권한만 가진다
    /// (같은 사용자의 다른 세션이 시작한 서비스에는 조회·대기만 허용된다). 연결 핸들을 쥐고 있어 PID는 재사용되지 않으며,
    /// 그래도 새 핸들의 생성 시각이 연결한 객체와 같고 아직 끝나지 않았으며 같은 사용자의 프로세스인지 확인한다.
    fn terminator(&self) -> Option<OwnedHandle> {
        let raw = unsafe { OpenProcess(PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE, 0, self.pid) };
        if raw.is_null() { return None; }
        let reopened = unsafe { OwnedHandle::from_raw_handle(raw as _) };
        let times = |handle: &OwnedHandle| -> Option<(u64, u64)> {
            let zero = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
            let (mut created, mut exited, mut kernel, mut user) = (zero, zero, zero, zero);
            (unsafe { GetProcessTimes(handle.as_raw_handle() as _, &mut created, &mut exited, &mut kernel, &mut user) } != 0)
                .then(|| ((u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime), (u64::from(exited.dwHighDateTime) << 32) | u64::from(exited.dwLowDateTime)))
        };
        let (original, current) = (times(&self.handle)?, times(&reopened)?);
        (original.0 != 0 && original == current && original.1 == 0 && aam_protocol::winutil::same_user_process(self.pid)).then_some(reopened)
    }
    fn stop(&self) -> bool {
        // 이미 끝난 프로세스에는 종료를 시도하지 않는다. 중요한 것은 종료됐다는 사실이라 결과는 종료 대기로 확인한다.
        // 중지 도중 충돌한 서비스도 종료로 본다. 관문이 있어 감독 프로세스가 되살리지 않는다.
        // 종료 권한을 얻지 못하거나 같은 객체임을 확인하지 못하면 종료하지 않고 실패를 돌려 호출자가 승인(permit)을 취소한다.
        if self.alive() {
            let Some(terminator) = self.terminator() else { return false };
            unsafe { TerminateProcess(terminator.as_raw_handle() as _, 0); }
        }
        unsafe { WaitForSingleObject(self.handle.as_raw_handle() as _, 10_000) == WAIT_OBJECT_0 }
    }
}

/// 감독 프로세스가 끝났다는 확인을 기다리는 최대 시간.
const SUPERVISOR_EXIT_WAIT: Duration = Duration::from_secs(15);
/// 감독 프로세스가 막 시작한 서비스의 pipe가 열리기를 기다리는 최대 시간.
const WORKER_STARTUP_WAIT: Duration = Duration::from_secs(30);

/// 관문을 쥔 채로 서비스에 연결한다. pipe가 아직 없어도 감독 프로세스가 살아 있고 서비스를 만들고 있을 수 있으면(막 시작 중) 기다린다.
/// 감독 프로세스가 없거나, 있어도 관문 때문에 멈춘 채 작업자가 없다고 스스로 증명하면(충돌 뒤 재시작 대기) 서비스가 없는 것이다.
/// 시작 중인 서비스를 시간 초과만으로 끝내지 않는다. 확인하지 못하면 아무것도 중지하지 않고 오류를 낸다.
fn connect_fenced(paths: &Paths, binary: &Path, fence: &mut aam_service::supervisor::Fence) -> Result<Option<ServiceProcess>, ApiError> {
    let deadline = Instant::now() + WORKER_STARTUP_WAIT;
    loop {
        if let Some(process) = ServiceProcess::connect(paths, binary)? {
            return Ok(Some(process));
        }
        if !fence.supervisor_alive() || fence.supervisor_idle() {
            return Ok(None);
        }
        if Instant::now() >= deadline {
            return Err(error("SERVICE_STARTING", "서비스가 시작되는 중이라 안전 확인을 끝내지 못했습니다. 아무것도 중지하지 않았습니다. 잠시 뒤 다시 시도해 주세요."));
        }
        thread::sleep(Duration::from_millis(100));
    }
}

/// 서비스는 응답하지 않는데 감독 프로세스만 남은 상태(충돌 뒤 재시작 대기 등)를 끝낸다. 새 감독 프로세스가 겹쳐 시작하지 않게 하고,
/// 서비스가 그 사이 다시 떴다면(승인한 적 없는 프로세스를 끄지 않도록) 아무것도 하지 않고 오류를 낸다.
/// 감독 프로세스가 작업자 없이 멈춰 있다고 증명한 뒤에만 끝내기로 확정한다. 증명하지 못하고 시간이 다하면 확정하지 않아 감독이 계속된다.
fn retire_idle_supervisor(paths: &Paths, binary: &Path) -> Result<(), ApiError> {
    let mut fence = aam_service::supervisor::engage(paths)?;
    let deadline = Instant::now() + SUPERVISOR_EXIT_WAIT;
    while fence.supervisor_alive() && !fence.supervisor_idle() {
        if ServiceProcess::connect(paths, binary)?.is_some() {
            return Err(error("SERVICE_RESTARTED_MEANWHILE", "서비스가 그 사이 다시 시작되어 건드리지 않았습니다. 다시 시도해 주세요."));
        }
        if Instant::now() >= deadline {
            return Err(error("SUPERVISOR_STOP_FAILED", "서비스 감독 프로세스가 작업자 없이 멈춘 것을 확인하지 못했습니다. 감독을 그대로 두었습니다."));
        }
        thread::sleep(Duration::from_millis(100));
    }
    if ServiceProcess::connect(paths, binary)?.is_some() {
        return Err(error("SERVICE_RESTARTED_MEANWHILE", "서비스가 그 사이 다시 시작되어 건드리지 않았습니다. 다시 시도해 주세요."));
    }
    fence.commit_retirement();
    fence.wait_supervisor_exit(SUPERVISOR_EXIT_WAIT)
}

/// `start_detached`로 시작한 프로세스(새 판은 감독 프로세스, 감독이 없던 이전 판은 서비스 자신)가 띄운 서비스를 pipe에서 찾는다.
/// 감독 프로세스는 서비스가 충돌하면 다시 띄우므로 서비스가 오기 전까지 기다린다. 시작한 프로세스도 감독 프로세스도 없으면 바로 실패한다.
fn started_worker(paths: &Paths, binary: &Path, started: &ServiceProcess) -> Result<ServiceProcess, ApiError> {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(worker) = ServiceProcess::connect(paths, binary)? {
            return Ok(worker);
        }
        if !started.alive() && !aam_service::supervisor::running(paths) {
            return Err(error("SERVICE_START_FAILED", "서비스가 준비되기 전에 시작한 프로세스가 종료됐습니다. 복구 기록을 보존했습니다."));
        }
        if Instant::now() >= deadline {
            return Err(error("SERVICE_START_TIMEOUT", "서비스 시작을 확인하지 못했습니다. 복구 기록을 보존했습니다."));
        }
        thread::sleep(Duration::from_millis(100));
    }
}
/// macOS와 같은 안전 절차: 신규 배정 차단과 lease 안전 검사(permit) → 서비스 종료 확인 → 기록 정리.
fn service_shutdown(paths: &Paths, remove_installation: bool) -> Result<String, ApiError> {
    let record_path = paths.home.join("service-install.json");
    let Some(mut record): Option<ServiceRecord> = read_owned(&record_path)? else {
        if !remove_installation {
            return Err(error("UNMANAGED_SERVICE", "앱 소유 서비스 설치 기록이 없어 안전하게 중지할 수 없습니다. 직접 실행한 서비스는 그 창에서 종료해 주세요."));
        }
        return Ok("앱 소유 서비스 설치 기록이 없습니다. 바꾼 것이 없습니다.".into());
    };
    if record.owner != OWNER {
        return Err(error("FOREIGN_INSTALL", "앱 소유 서비스 기록이 아니므로 제거하지 않습니다."));
    }
    let _fence = retire_registered(paths, &record_path, &mut record, false)?;
    if !remove_installation {
        return Ok("신규 배정과 활성·불확실 lease의 안전 조건을 확인한 뒤 관리 서비스를 중지했습니다. 자동 실행 등록과 계정·프로필·로그는 보존했습니다.".into());
    }
    let key = open_key(RUN_KEY, true)?;
    if read_value(&key, LEGACY_RUN_VALUE)?.is_some_and(|(value, _)| value == record.run_value) {
        delete_value(&key, LEGACY_RUN_VALUE)?;
    }
    if read_value(&key, RUN_VALUE)?.is_some_and(|(value, _)| value == record.run_value) {
        delete_value(&key, RUN_VALUE)?;
    }
    fs::remove_file(record_path).map_err(io_error)?;
    Ok("신규 배정과 활성·불확실 lease의 안전 조건을 확인한 뒤 자동 실행 등록만 제거했습니다. 계정·프로필·로그는 보존했습니다.".into())
}

/// 앱이 등록한 서비스를 내려 실행 파일을 바꿀 수 있는 상태로 만들고, 그 관문을 돌려준다. 호출자는 파일 작업이 끝날 때까지 관문을 쥔다.
/// 먼저 관문을 걸어(감독 프로세스는 작업자를 만들지 않고 멈춘다) 서비스 pipe를 찾는다. 서비스가 있으면 같은 프로세스 객체에 대해 lease 안전 검사(permit)와
/// 종료를 하고, 감독 프로세스를 끝내기로 확정한다. 서비스가 없을 때도 시작 잠금과 저장소를 직접 확인해 점유 lease가 없을 때만 승인한다.
/// `rollback`은 파일을 되돌리고 서비스를 다시 시작해 lease를 새로 확인하므로 이 종료 승인을 만들지 않는다.
/// 거절·실패로 돌아가면 감독 프로세스는 확정하지 않았으므로 관문을 놓는 순간 작업자 감독을 이어 가서 자동 복구가 살아 있다.
fn retire_registered(paths: &Paths, record_path: &Path, record: &mut ServiceRecord, rollback: bool) -> Result<aam_service::supervisor::Fence, ApiError> {
    let mut fence = aam_service::supervisor::engage(paths)?;
    if let Some(process) = connect_fenced(paths, &record.binary_path, &mut fence)? {
        let (permit, reboot_proven) = match process.prepare(paths) {
            Ok(permit) => (permit, false),
            // 재부팅 규칙이 없던 이전 서비스는 재부팅으로 끝난 세션도 붙잡고 종료를 거절한다. 같은 쓰기 트랜잭션에서
            // 점유 전부가 부팅 ID로 이전 부팅임이 증명될 때만 신규 배정을 닫는다(실행 중 서비스도 배정마다 이 값을 읽는다).
            Err(failure) if failure.code == "SESSION_BUSY" && !rollback => {
                match aam_service::close_admission_if_busy_only_from_previous_boot(paths)? {
                    Some(permit) => (permit, true),
                    None => return Err(failure),
                }
            }
            Err(failure) => return Err(failure),
        };
        record.uninstall_permit = Some(permit);
        save(record_path, record)?;
        if reboot_proven {
            // 승인 저장 직전에 배정 검사를 이미 통과한 요청이 있을 수 있다. 배정은 같은 저장소 쓰기 잠금 아래에서만 lease를 만들므로,
            // 승인을 저장한 뒤 다시 한 번 같은 증명을 거치면 그 사이 생긴 lease도 보인다. 하나라도 있으면 끄지 않고 취소한다.
            if aam_service::close_admission_if_busy_only_from_previous_boot(paths)?.as_ref() != record.uninstall_permit.as_ref() {
                if process.call(paths, "service.cancelUninstall", json!({ "permit": record.uninstall_permit })).is_ok() {
                    record.uninstall_permit = None;
                    save(record_path, record)?;
                }
                return Err(error("SESSION_BUSY", "새 대화가 시작되어 서비스를 멈추지 않았습니다. 대화를 마친 뒤 다시 시도해 주세요."));
            }
            if !process.stop() || running(paths) {
                if process.call(paths, "service.cancelUninstall", json!({ "permit": record.uninstall_permit })).is_ok() {
                    record.uninstall_permit = None;
                    save(record_path, record)?;
                }
                return Err(error("SERVICE_STOP_FAILED", "서비스 종료를 확인하지 못했습니다. 설치 기록은 보존했습니다."));
            }
            drop(process);
            // 꺼진 상태에서 시작 잠금을 쥐고 같은 규칙으로 재검증·반환한다. 같은 승인 값을 이어 쓴다.
            record.uninstall_permit = Some(aam_service::prepare_stopped_uninstall(paths)?);
            save(record_path, record)?;
            fence.commit_retirement();
            fence.wait_supervisor_exit(SUPERVISOR_EXIT_WAIT)?;
            return Ok(fence);
        }
        let stopped = process.stop();
        if !stopped || running(paths) {
            if process.call(paths, "service.cancelUninstall", json!({ "permit": record.uninstall_permit })).is_ok() {
                record.uninstall_permit = None;
                save(record_path, record)?;
            }
            return Err(error("SERVICE_STOP_FAILED", "서비스 종료를 확인하지 못했습니다. 설치 기록은 보존했습니다."));
        }
    } else if !rollback && paths.database.try_exists().map_err(io_error)? {
        record.uninstall_permit = Some(aam_service::prepare_stopped_uninstall(paths)?);
        save(record_path, record)?;
    }
    fence.commit_retirement();
    fence.wait_supervisor_exit(SUPERVISOR_EXIT_WAIT)?;
    Ok(fence)
}

/// 업데이트 복구가 파일을 되돌리기 전에 등록된 서비스와 감독 프로세스를 끝낸다. 파일 복구가 끝날 때까지 관문을 쥐고 돌려준다.
/// 서비스 pipe가 없어도(충돌 뒤 재시작 대기 중인 감독 프로세스) 감독 프로세스를 독립적으로 끝내고 확인한다.
pub(super) fn service_stop_for_rollback(paths: &Paths) -> Result<aam_service::supervisor::Fence, ApiError> {
    let record_path = paths.home.join("service-install.json");
    let mut record: ServiceRecord = read_owned(&record_path)?
        .ok_or_else(|| error("UNMANAGED_SERVICE", "앱 소유 서비스 설치 기록이 없어 중지하지 않았습니다."))?;
    if record.owner != OWNER {
        return Err(error("FOREIGN_INSTALL", "앱 소유 서비스 기록이 아니므로 중지하지 않습니다."));
    }
    retire_registered(paths, &record_path, &mut record, true)
}


/// 앱 업데이트 뒤 서비스 교체(macOS `launchctl kickstart -k`에 해당). 앱이 등록한 서비스일 때만 하고,
/// 등록 기록이 없으면 아무것도 하지 않는다. 종료 확인 뒤 기록된 실행 파일로 다시 시작한다.
pub fn service_restart(paths: &Paths) -> Result<(), ApiError> {
    let Some(mut record): Option<ServiceRecord> = read_owned(&paths.home.join("service-install.json"))? else {
        return Ok(());
    };
    if record.owner != OWNER {
        return Err(error("FOREIGN_INSTALL", "앱 소유 서비스 기록이 아니므로 재시작하지 않습니다."));
    }
    if running(paths) {
        service_stop(paths)?;
    } else if aam_service::supervisor::running(paths) {
        retire_idle_supervisor(paths, &record.binary_path)?;
    }
    start_detached(&record.binary_path, paths)?;
    let deadline = Instant::now() + Duration::from_secs(15);
    while !running(paths) {
        if Instant::now() >= deadline {
            return Err(error("SERVICE_START_TIMEOUT", "서비스 재시작을 확인하지 못했습니다."));
        }
        thread::sleep(Duration::from_millis(200));
    }
    call(paths, "service.resumeAdmission", json!({}))?;
    record.uninstall_permit = None;
    save(&paths.home.join("service-install.json"), &record)?;
    Ok(())
}

/// 다른 설치의 서비스는 사용 중지하지 않는다. 기록 없는 실행 중 서비스도 건드리지 않는다.
pub fn confirm_removal_target(paths: &Paths, install_dir: &Path) -> Result<(), ApiError> {
    let install_dir = installation_directory(install_dir)?;
    if let Some(record) = read_owned::<ServiceRecord>(&paths.home.join("service-install.json"))? {
        if record.owner != OWNER || record.binary_path.parent().and_then(|path| path.canonicalize().ok()).as_ref() != Some(&install_dir) {
            return Err(error("FOREIGN_INSTALL", "다른 설치가 관리하는 서비스는 제거하지 않습니다."));
        }
    } else if running(paths) || aam_service::supervisor::running(paths) {
        return Err(error("UNMANAGED_SERVICE", "등록되지 않은 실행 중 서비스가 있어 제거하지 않았습니다."));
    }
    Ok(())
}

/// 앱이 만든 HKCU Run `Ojak`만 지운다. 값이 이 설치 폴더의 Ojak.exe를 가리킬 때만 앱 소유로 본다.
pub fn remove_app_autostart(install_dir: &Path) -> Result<(), ApiError> {
    let install_dir = installation_directory(install_dir)?;
    let key = open_key(RUN_KEY, true)?;
    let Some((value, _)) = read_value(&key, APP_RUN_VALUE)? else {
        return Ok(());
    };
    if app_owns_autostart(&value, &install_dir) {
        delete_value(&key, APP_RUN_VALUE)?;
    }
    Ok(())
}

/// Run 값이 이 설치 폴더 바로 아래 앱 실행 파일을 가리킬 때만 앱 소유로 본다.
/// 값은 문자열 경계만 쓰는 안전한 파싱으로 읽는다(바이트 인덱스 슬라이싱 없음, 어떤 입력에도 패닉 없음).
fn app_owns_autostart(value: &str, install_dir: &Path) -> bool {
    let dir = path_segments(&install_dir.to_string_lossy());
    if dir.is_empty() {
        return false;
    }
    let value = value.trim();
    if let Some(rest) = value.strip_prefix('"') {
        return rest.find('"').and_then(|end| rest.get(..end)).is_some_and(|program| is_app_in_dir(program, &dir));
    }
    // 따옴표 없는 값은 공백이 경로일 수도 인자 시작일 수도 있어, 공백 위치마다 앞부분을 프로그램 경로 후보로 본다.
    value
        .char_indices()
        .filter(|(_, ch)| ch.is_whitespace())
        .filter_map(|(index, _)| value.get(..index))
        .chain(std::iter::once(value))
        .any(|program| is_app_in_dir(program, &dir))
}

/// `program`이 `dir`(정규화된 구성 요소) 바로 아래의 앱 실행 파일이면 true.
fn is_app_in_dir(program: &str, dir: &[String]) -> bool {
    let segments = path_segments(program);
    match segments.split_last() {
        Some((name, parent)) if is_app_exe(name) => parent == dir
            || Path::new(program).parent().and_then(|path| installation_directory(path).ok())
                .is_some_and(|resolved| path_segments(&resolved.to_string_lossy()) == dir),
        Some(_) => false,
        None => false,
    }
}

/// 경로를 대소문자 무시·구분자 무시(`\`와 `/`, 연속·끝 구분자)로 구성 요소로 나눈다.
/// `\\?\` 확장 접두사는 벗기고, `\\?\UNC\`는 일반 UNC 경로와 같게 본다.
fn path_segments(path: &str) -> Vec<String> {
    let path = path.trim();
    let (unc, rest) = if let Some(rest) = path.strip_prefix(r"\\?\UNC\").or_else(|| path.strip_prefix(r"\\?\unc\")) {
        (true, rest)
    } else if let Some(rest) = path.strip_prefix(r"\\?\").or_else(|| path.strip_prefix(r"\??\")) {
        (false, rest)
    } else {
        (path.starts_with(r"\\") || path.starts_with("//"), path)
    };
    let mut segments = Vec::new();
    if unc {
        segments.push(r"\\".to_owned());
    }
    segments.extend(rest.split(['\\', '/']).filter(|part| !part.is_empty()).map(str::to_lowercase));
    segments
}

/// 설치본 앱 실행 파일 이름. 번들은 Cargo 이름 `ai-account-manager.exe`로 설치되고(실기기 Run 값으로 확인),
/// 제품명 `Ojak.exe`로 설치되는 빌드도 있어 둘 다 앱으로 본다. 서비스(`aam-service.exe`)는 앱이 아니다.
fn is_app_exe(name: &str) -> bool {
    name.eq_ignore_ascii_case("ai-account-manager.exe") || name.eq_ignore_ascii_case("Ojak.exe")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aam_cmd_install_and_uninstall_are_symmetric() {
        let root = std::env::temp_dir().join(format!("aam-shim-{}", aam_protocol::new_id()));
        let bin = root.join("bin");
        fs::create_dir_all(&bin).unwrap();
        let launcher = PathBuf::from(r"C:\Ojak\aam.exe");
        place_shim(&launcher, &bin, "aam").unwrap();
        place_shim(&launcher, &bin, "claude").unwrap();
        let aam = fs::read_to_string(shim_file(&bin, "aam")).unwrap();
        let claude = fs::read_to_string(shim_file(&bin, "claude")).unwrap();
        assert!(aam.contains(r"C:\Ojak\aam.exe"), "{aam}");
        assert!(!aam.contains("--shim"), "{aam}");
        assert!(claude.contains("--shim claude"), "{claude}");
        place_shim(&launcher, &bin, "aam").unwrap();
        assert_eq!(fs::read_to_string(shim_file(&bin, "aam")).unwrap(), aam);
        remove_shim(&bin, "aam").unwrap();
        remove_shim(&bin, "claude").unwrap();
        assert!(!shim_file(&bin, "aam").exists());
        assert!(!legacy_shim(&bin, "aam").exists());
        assert!(!shim_file(&bin, "claude").exists());
        remove_shim(&bin, "aam").unwrap();
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn app_autostart_ownership_ignores_other_programs() {
        let dir = PathBuf::from(r"C:\Users\me\AppData\Local\Programs\Ojak");
        assert!(app_owns_autostart(r"C:\Users\me\AppData\Local\Programs\Ojak\Ojak.exe --autostart", &dir));
        assert!(app_owns_autostart(r#""C:\Users\me\AppData\Local\Programs\Ojak\Ojak.exe" --autostart"#, &dir));
        assert!(!app_owns_autostart(r"C:\Other\Ojak.exe --autostart", &dir));
        assert!(!app_owns_autostart(r"C:\Users\me\AppData\Local\Programs\Ojak\aam-service.exe", &dir));
        // 실제 설치본은 Cargo 이름으로 깔린다(Windows 실기기 HKCU Run 값).
        let installed = PathBuf::from(r"C:\Users\me\AppData\Local\Ojak");
        assert!(app_owns_autostart(r"C:\Users\me\AppData\Local\Ojak\ai-account-manager.exe --autostart", &installed));
        assert!(app_owns_autostart(r#""C:\Users\me\AppData\Local\Ojak\ai-account-manager.exe" --autostart"#, &installed));
        assert!(!app_owns_autostart(r"C:\Elsewhere\ai-account-manager.exe --autostart", &installed));
    }
}

#[cfg(test)]
mod autostart_ownership_tests {
    use super::*;

    #[test]
    fn foreign_prefix_directory_is_not_owned() {
        let dir = PathBuf::from(r"C:\Ojak");
        assert!(!app_owns_autostart(r"C:\Ojakai-account-manager.exe --autostart", &dir));
        assert!(!app_owns_autostart(r"C:\OjakOjak.exe", &dir));
        assert!(!app_owns_autostart(r#""C:\Ojakai-account-manager.exe" --autostart"#, &dir));
        assert!(app_owns_autostart(r"C:\Ojak\ai-account-manager.exe --autostart", &dir));
    }

    #[test]
    fn non_ascii_foreign_path_does_not_panic_and_is_not_owned() {
        let dir = PathBuf::from(r"C:\Ojak");
        assert!(!app_owns_autostart(r"C:\오작폴더\Other.exe", &dir));
        assert!(!app_owns_autostart(r#""C:\오작폴더\Other.exe" --autostart"#, &dir));
        // 설치 폴더보다 짧은 값, 따옴표만 있는 값, 빈 값도 패닉하지 않는다.
        let long = PathBuf::from(r"C:\오작\아주\긴\설치\폴더");
        assert!(!app_owns_autostart("오", &long));
        assert!(!app_owns_autostart("\"", &long));
        assert!(!app_owns_autostart("", &long));
        assert!(!app_owns_autostart("   ", &dir));
    }

    #[test]
    fn non_ascii_install_dir_with_real_exe_is_owned() {
        let dir = PathBuf::from(r"C:\Users\오작\Ojak");
        assert!(app_owns_autostart(r"C:\Users\오작\Ojak\ai-account-manager.exe --autostart", &dir));
        assert!(app_owns_autostart(r#""C:\Users\오작\Ojak\ai-account-manager.exe" --autostart"#, &dir));
        assert!(app_owns_autostart(r"C:\Users\오작\Ojak\Ojak.exe", &dir));
        assert!(!app_owns_autostart(r"C:\Users\오작\Ojak2\Ojak.exe", &dir));
    }

    #[test]
    fn service_exe_is_not_owned() {
        let dir = PathBuf::from(r"C:\Users\me\AppData\Local\Ojak");
        assert!(!app_owns_autostart(r"C:\Users\me\AppData\Local\Ojak\aam-service.exe", &dir));
        assert!(!app_owns_autostart(r#""C:\Users\me\AppData\Local\Ojak\aam-service.exe" --autostart"#, &dir));
        assert!(!app_owns_autostart(r"C:\Users\me\AppData\Local\Ojak\aam.exe", &dir));
    }

    #[test]
    fn exe_must_be_directly_inside_install_dir() {
        let dir = PathBuf::from(r"C:\Ojak");
        assert!(!app_owns_autostart(r"C:\Ojak\sub\Ojak.exe", &dir));
        assert!(!app_owns_autostart(r"C:\Ojak.exe", &dir));
        assert!(!app_owns_autostart(r"C:\Ojak", &dir));
    }

    #[test]
    fn separator_case_and_prefix_variants_are_owned() {
        let dir = PathBuf::from(r"C:\Users\me\Ojak\");
        assert!(app_owns_autostart(r"C:\Users\me\Ojak\Ojak.exe", &dir));
        assert!(app_owns_autostart(r"C:/Users/me/Ojak/Ojak.exe --autostart", &dir));
        assert!(app_owns_autostart(r"c:\USERS\ME\ojak\OJAK.EXE --autostart", &dir));
        assert!(app_owns_autostart(r#""c:\users\me\OJAK\ai-account-manager.EXE" --autostart"#, &dir));
        assert!(app_owns_autostart(r"C:\Users\me\Ojak\\Ojak.exe", &dir));
        let canonical = PathBuf::from(r"\\?\C:\Users\me\Ojak");
        assert!(app_owns_autostart(r"C:\Users\me\Ojak\Ojak.exe --autostart", &canonical));
        assert!(app_owns_autostart(r#""\\?\C:\Users\me\Ojak\Ojak.exe""#, &dir));
        assert!(!app_owns_autostart(r"C:\Users\me\Other\Ojak.exe", &canonical));
    }

    #[test]
    fn paths_with_spaces_and_arguments_are_parsed() {
        let dir = PathBuf::from(r"C:\Program Files\Ojak");
        assert!(app_owns_autostart(r"C:\Program Files\Ojak\Ojak.exe --autostart", &dir));
        assert!(app_owns_autostart(r#""C:\Program Files\Ojak\Ojak.exe" --autostart --flag "x y""#, &dir));
        assert!(!app_owns_autostart(r#""C:\Program Files\Other\Ojak.exe" --autostart"#, &dir));
        // 인자에 앱 경로가 들어 있어도 프로그램 자체가 다르면 소유가 아니다.
        assert!(!app_owns_autostart(r#""C:\Other\run.exe" "C:\Program Files\Ojak\Ojak.exe""#, &dir));
        assert!(!app_owns_autostart(r"C:\Other\run.exe C:\Program Files\Ojak\Ojak.exe", &dir));
    }

    #[test]
    fn unc_paths_compare_like_extended_unc() {
        let dir = PathBuf::from(r"\\?\UNC\server\share\Ojak");
        assert!(app_owns_autostart(r"\\server\share\Ojak\Ojak.exe --autostart", &dir));
        assert!(!app_owns_autostart(r"C:\server\share\Ojak\Ojak.exe", &dir));
    }
}
