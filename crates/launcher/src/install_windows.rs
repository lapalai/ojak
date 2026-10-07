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
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::{CloseHandle, ERROR_FILE_NOT_FOUND, WAIT_OBJECT_0},
    System::{
        Registry::{
            RegCloseKey, RegCreateKeyExW, RegDeleteValueW, RegGetValueW, RegOpenKeyExW, RegSetValueExW, HKEY,
            HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_READ, KEY_WRITE, REG_EXPAND_SZ, REG_OPTION_NON_VOLATILE,
            REG_SZ, RRF_NOEXPAND, RRF_RT_REG_EXPAND_SZ, RRF_RT_REG_SZ,
        },
        Threading::{
            CreateProcessW, OpenProcess, TerminateProcess, WaitForSingleObject,
            CREATE_BREAKAWAY_FROM_JOB, CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW,
            DETACHED_PROCESS, PROCESS_INFORMATION, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE,
            STARTUPINFOW,
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
fn start_detached(binary: &Path, paths: &Paths) -> Result<(), ApiError> {
    let command = wide(&format!("\"{}\"", binary.display()));
    let mut command = command;
    let mut startup: STARTUPINFOW = unsafe { std::mem::zeroed() };
    startup.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
    let mut info: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
    // 환경(AAM_HOME 포함)은 이 실행기에서 물려받는다.
    let _ = paths;
    let base = DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW;
    for flags in [base | CREATE_BREAKAWAY_FROM_JOB, base] {
        let ok = unsafe {
            CreateProcessW(
                std::ptr::null(),
                command.as_mut_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                0,
                flags,
                std::ptr::null(),
                std::ptr::null(),
                &startup,
                &mut info,
            )
        };
        if ok != 0 {
            unsafe {
                CloseHandle(info.hThread);
                CloseHandle(info.hProcess);
            }
            return Ok(());
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
    if !running(paths) {
        start_detached(&binary, paths)?;
        let deadline = Instant::now() + Duration::from_secs(15);
        while !running(paths) {
            if Instant::now() >= deadline {
                return Err(error("SERVICE_START_TIMEOUT", "로그인 시 자동 실행은 등록했지만 서비스 시작을 확인하지 못했습니다."));
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
        "로그인 시 자동 실행: {} · 서비스: {}",
        if registered { "등록됨" } else { "미등록" },
        if running(paths) { "실행 중" } else { "중지됨" }
    ))
}

pub fn service_stop(paths: &Paths) -> Result<String, ApiError> {
    service_shutdown(paths, false)
}

pub fn service_uninstall(paths: &Paths) -> Result<String, ApiError> {
    service_shutdown(paths, true)
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
    if running(paths) {
        let prepared = call(paths, "service.prepareUninstall", json!({}))?;
        let permit = prepared.get("permit").and_then(|v| v.as_str()).filter(|v| !v.is_empty());
        if prepared.get("safe").and_then(|v| v.as_bool()) != Some(true) || permit.is_none() {
            return Err(error("INVALID_UNINSTALL_PERMIT", "신규 배정 차단과 lease 안전 검사를 확인하지 못해 서비스를 중지하지 않았습니다."));
        }
        record.uninstall_permit = permit.map(str::to_owned);
        save(&record_path, &record)?;
        let stopped = aam_protocol::service_pid(paths).ok().is_some_and(terminate);
        if !stopped || running(paths) {
            if call(paths, "service.cancelUninstall", json!({ "permit": record.uninstall_permit })).is_ok() {
                record.uninstall_permit = None;
                save(&record_path, &record)?;
            }
            return Err(error("SERVICE_STOP_FAILED", "서비스 종료를 확인하지 못했습니다. 설치 기록은 보존했습니다."));
        }
    } else if paths.database.exists() && record.uninstall_permit.is_none() {
        return Err(error("LEASE_STATE_UNKNOWN", "서비스가 꺼져 있지만 저장된 lease의 생존 여부를 확인할 수 없습니다. 서비스를 다시 시작하여 안전하게 제거하세요."));
    }
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

fn terminate(pid: u32) -> bool {
    unsafe {
        let process = OpenProcess(PROCESS_TERMINATE | PROCESS_SYNCHRONIZE, 0, pid);
        if process.is_null() {
            return false;
        }
        let ok = TerminateProcess(process, 0) != 0 && WaitForSingleObject(process, 10_000) == WAIT_OBJECT_0;
        CloseHandle(process);
        ok
    }
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
    let install_dir = update::directory(install_dir)?;
    if let Some(record) = read_owned::<ServiceRecord>(&paths.home.join("service-install.json"))? {
        if record.owner != OWNER || record.binary_path.parent().and_then(|path| path.canonicalize().ok()).as_ref() != Some(&install_dir) {
            return Err(error("FOREIGN_INSTALL", "다른 설치가 관리하는 서비스는 제거하지 않습니다."));
        }
    } else if running(paths) {
        return Err(error("UNMANAGED_SERVICE", "등록되지 않은 실행 중 서비스가 있어 제거하지 않았습니다."));
    }
    Ok(())
}

/// 앱이 만든 HKCU Run `Ojak`만 지운다. 값이 이 설치 폴더의 Ojak.exe를 가리킬 때만 앱 소유로 본다.
pub fn remove_app_autostart(install_dir: &Path) -> Result<(), ApiError> {
    let install_dir = update::directory(install_dir)?;
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
        Some((name, parent)) => parent == dir && is_app_exe(name),
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
