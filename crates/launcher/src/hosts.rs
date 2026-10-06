use crate::install::{shell_configured, shim_installed};
use aam_protocol::{ApiError, Paths};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    io::Read,
    path::{Path, PathBuf},
};

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HostConnections {
    pub shim_directory: String,
    pub shims: Vec<ShimConnection>,
    pub hosts: Vec<HostConnection>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ShimConnection {
    pub tool: String,
    pub path: String,
    pub installed: bool,
}

/// 화면이 표시 언어로 옮기는 안내 한 줄. 문장은 데스크톱 사전(`hosts.note.<key>`)에 있고, 여기는 키와 값만 보낸다.
#[derive(Serialize, Clone)]
pub struct HostNote {
    pub key: &'static str,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub params: BTreeMap<&'static str, String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HostConnection {
    pub id: String,
    pub name: String,
    pub installed: bool,
    pub status: &'static str,
    /// 첫 줄이 현재 상태 요약이고, 나머지는 자세히 볼 때 펼치는 안내다.
    pub notes: Vec<HostNote>,
    pub commands: Vec<HostCommand>,
}

#[derive(Serialize)]
pub struct HostCommand {
    pub tool: String,
    pub command: String,
    pub setting: String,
}

fn note(key: &'static str) -> HostNote {
    HostNote { key, params: BTreeMap::new() }
}

fn note_with(key: &'static str, params: &[(&'static str, String)]) -> HostNote {
    HostNote { key, params: params.iter().cloned().collect() }
}

// 설정에 포함된 다른 정보는 역직렬화하거나 응답에 노출하지 않습니다.
#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct OrcaSettings {
    #[serde(default)]
    agent_cmd_overrides: BTreeMap<String, String>,
    experimental_native_chat: Option<bool>,
    experimental_structured_native_chat: Option<bool>,
}

#[derive(Deserialize)]
struct OrcaData {
    settings: OrcaSettings,
}

/// 읽지 못한 이유(권한·형식·크기·해석)는 사용자에게 같은 조치로 이어지므로 구분하지 않는다.
fn orca_settings(path: &Path) -> Result<Option<OrcaSettings>, ()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(()),
    };
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > 8 * 1024 * 1024 {
        return Err(());
    }
    let file = fs::File::open(path).map_err(|_| ())?;
    serde_json::from_reader::<_, OrcaData>(file.take(8 * 1024 * 1024))
        .map(|data| Some(data.settings))
        .map_err(|_| ())
}

#[derive(Deserialize)]
struct ConductorSettings {
    claude_code_executable_path: Option<String>,
}

fn conductor_setting(path: &Path) -> Result<Option<String>, ()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(()),
    };
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > 1024 * 1024 {
        return Err(());
    }
    let mut text = String::new();
    fs::File::open(path)
        .map_err(|_| ())?
        .take(1024 * 1024 + 1)
        .read_to_string(&mut text)
        .map_err(|_| ())?;
    if text.len() > 1024 * 1024 {
        return Err(());
    }
    // 실행 경로 한 필드만 유지합니다. 파서 오류에는 다른 설정 값이 포함될 수 있어 노출하지 않습니다.
    let settings: ConductorSettings = if path.extension().is_some_and(|extension| extension == "toml") {
        toml::from_str(&text).map_err(|_| ())?
    } else {
        serde_json::from_str(&text).map_err(|_| ())?
    };
    Ok(settings.claude_code_executable_path)
}

fn conductor_configuration(home: &Path, bin: &Path) -> (bool, Vec<HostNote>) {
    let project = std::env::current_dir().ok().and_then(|cwd| {
        cwd.ancestors()
            .find(|path| path.join(".git").exists())
            .map(Path::to_path_buf)
    });
    // 상위 계층의 값이 먼저 적용되며, 같은 계층에서는 TOML이 legacy JSON보다 우선합니다.
    let mut layers = vec![(home.join(".conductor/settings.managed.toml"), "managed")];
    if let Some(project) = &project {
        layers.push((project.join(".conductor/settings.local.toml"), "projectLocal"));
        layers.push((project.join(".conductor/settings.toml"), "projectShared"));
    }
    layers.push((home.join(".conductor/settings.toml"), "user"));
    let scope = project.as_ref().map_or_else(
        || note("conductorScopeNone"),
        |path| note_with("conductorScopeProject", &[("path", path.display().to_string())]),
    );
    for (toml_path, layer) in layers {
        for path in [toml_path.clone(), toml_path.with_extension("json")] {
            match conductor_setting(&path) {
                Ok(Some(value)) => {
                    let configured = Path::new(&value) == bin.join("claude");
                    let key = if configured { "conductorShim" } else { "conductorOther" };
                    return (configured, vec![note_with(key, &[("layer", layer.into())]), scope]);
                }
                Ok(None) => {}
                Err(()) => return (false, vec![note_with("conductorFailed", &[("layer", layer.into())]), scope]),
            }
        }
    }
    (false, vec![note("conductorMissing"), scope])
}

/// 사용자 앱 데이터 폴더(macOS `~/Library/Application Support`, Windows `%APPDATA%`).
fn app_data(home: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_else(|| home.join("AppData/Roaming"))
    }
    #[cfg(unix)]
    {
        home.join("Library/Application Support")
    }
}

/// 호스트 앱 설치 여부. macOS는 앱 번들, Windows는 사용자·시스템 설치 위치의 실행 파일로 확인한다.
fn installed(home: &Path, mac_app: &str, windows_exe: &str) -> bool {
    #[cfg(windows)]
    {
        let _ = mac_app;
        let user = std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(|| home.join("AppData/Local"));
        let mut roots = vec![user.join("Programs")];
        roots.extend(std::env::var_os("ProgramFiles").map(PathBuf::from));
        roots.iter().any(|root| root.join(windows_exe).is_file())
    }
    #[cfg(unix)]
    {
        let _ = windows_exe;
        Path::new("/Applications").join(mac_app).is_dir() || home.join("Applications").join(mac_app).is_dir()
    }
}

/// shim 실행 파일 경로(Windows는 `.cmd` 배치 파일).
fn shim_path(bin: &Path, tool: &str) -> PathBuf {
    if cfg!(windows) { bin.join(format!("{tool}.cmd")) } else { bin.join(tool) }
}

/// 복사해 붙여 넣는 실행 명령. macOS는 POSIX shell 따옴표, Windows는 cmd·호스트 설정용 큰따옴표를 쓴다.
fn launch_text(path: &Path) -> String {
    if cfg!(windows) {
        format!("\"{}\"", path.display())
    } else {
        crate::install::shell_quote(path)
    }
}

fn command(bin: &Path, tool: &str, setting: impl Into<String>) -> HostCommand {
    HostCommand {
        tool: tool.into(),
        command: launch_text(&shim_path(bin, tool)),
        setting: setting.into(),
    }
}

fn terminal_commands(bin: &Path) -> Vec<HostCommand> {
    ["claude", "codex"]
        .into_iter()
        .map(|tool| command(bin, tool, "통합 터미널에서 직접 실행 (절대 경로로 PATH·호스트 wrapper 우회)"))
        .collect()
}

fn connection(
    id: &str,
    name: &str,
    installed: bool,
    configured: bool,
    mut notes: Vec<HostNote>,
    commands: Vec<HostCommand>,
) -> HostConnection {
    if !installed {
        notes.insert(0, note("appMissing"));
    }
    notes.push(note("staticOnly"));
    HostConnection {
        id: id.into(),
        name: name.into(),
        installed,
        status: if !installed {
            "unavailable"
        } else if configured {
            "configured"
        } else {
            "setup-required"
        },
        notes,
        commands,
    }
}

pub fn host_connections(paths: &Paths) -> Result<HostConnections, ApiError> {
    if !paths.home.is_absolute() {
        return Err(ApiError::new(
            "INVALID_PATH",
            "앱 관리 경로는 절대 경로여야 합니다.",
        ));
    }
    let home = aam_protocol::user_home()
        .ok_or_else(|| ApiError::new("HOME_MISSING", "사용자 홈 경로를 확인하지 못했습니다."))?;
    let bin = paths.home.join("bin");
    let shims = ["claude", "codex"]
        .into_iter()
        .map(|tool| {
            Ok(ShimConnection {
                tool: tool.into(),
                path: shim_path(&bin, tool).to_string_lossy().into_owned(),
                installed: shim_installed(paths, tool)?,
            })
        })
        .collect::<Result<Vec<_>, ApiError>>()?;
    let all_shims = shims.iter().all(|shim| shim.installed);
    let (shell_ready, shell_note) = match shell_configured(paths) {
        Ok(true) => (true, note("shellReady")),
        Ok(false) => (false, note_with("shellMissing", &[("command", launch_text(&shim_path(&bin, "aam")))])),
        Err(error) => (false, note_with("shellFailed", &[("message", error.message)])),
    };
    let preparation = || note(if all_shims { "shimsReady" } else { "shimsMissing" });
    let orca_path = app_data(&home).join("Orca/profiles/local-default/orca-data.json");
    let (orca_ready, orca_note) = match orca_settings(&orca_path) {
        Ok(Some(settings)) => {
            let native = settings.experimental_native_chat == Some(true) || settings.experimental_structured_native_chat == Some(true);
            let ready = !native && ["claude", "codex"].into_iter().all(|tool| {
                match settings.agent_cmd_overrides.get(tool).map(|value| value.trim()).filter(|value| !value.is_empty()) {
                    Some(value) => {
                        let path = shim_path(&bin, tool);
                        value == launch_text(&path) || value == path.to_string_lossy()
                    }
                    None => shell_ready,
                }
            });
            (ready, note(if native { "orcaNative" } else { "orcaTui" }))
        }
        Ok(None) => (false, note("orcaNoSettings")),
        Err(()) => (false, note("orcaUnreadable")),
    };
    let mut hosts = vec![connection(
        "orca", "Orca", installed(&home, "Orca.app", "orca/Orca.exe"), all_shims && orca_ready,
        vec![orca_note, preparation(), shell_note.clone(), note("orcaCache")],
        ["claude", "codex"].into_iter().map(|tool| command(&bin, tool, format!("Orca 설정의 TUI 명령 override: settings.agentCmdOverrides.{tool} (선택한 Claude 프로필은 변경하지 않음)"))).collect(),
    )];
    // Superset·cmux·Conductor는 macOS 앱만 있다. Windows에서는 행을 만들지 않는다.
    if cfg!(unix) {
        hosts.push(connection(
            "superset", "Superset", installed(&home, "Superset.app", ""), false,
            vec![note("superset"), preparation()],
            vec![command(&bin, "claude", "Settings → Agents → Claude Code → Command (No Prompt) 및 Command (With Prompt)의 실행 명령; prompt 전달 설정 유지"), command(&bin, "codex", "Superset 터미널에서 직접 실행; Superset Chat 대체 아님")],
        ));
        hosts.push(connection(
            "cmux", "cmux", installed(&home, "cmux.app", ""), false,
            vec![note("cmux"), preparation(), shell_note.clone()],
            terminal_commands(&bin),
        ));
        let (conductor_ready, mut conductor_notes) = conductor_configuration(&home, &bin);
        let conductor_value = serde_json::to_string(&bin.join("claude").to_string_lossy())
            .map_err(|_| ApiError::new("INVALID_PATH", "설정용 CLI 경로를 표시하지 못했습니다."))?;
        conductor_notes.push(preparation());
        conductor_notes.push(note_with("conductorHow", &[("value", conductor_value)]));
        hosts.push(connection(
            "conductor", "Conductor", installed(&home, "Conductor.app", ""), shims.iter().any(|shim| shim.tool == "claude" && shim.installed) && conductor_ready,
            conductor_notes,
            vec![command(&bin, "claude", "claude_code_executable_path에는 위 TOML 경로 값을 사용; 이 명령은 터미널 실행용")],
        ));
    }
    for (id, name, app, exe, config) in [
        ("vscode", "VS Code", "Visual Studio Code.app", "Microsoft VS Code/Code.exe", "Code"),
        ("cursor", "Cursor", "Cursor.app", "cursor/Cursor.exe", "Cursor"),
    ] {
        let settings_exist = app_data(&home).join(config).join("User/settings.json").is_file();
        hosts.push(connection(
            id, name, installed(&home, app, exe), all_shims && shell_ready,
            vec![note("ideTerminal"), preparation(), shell_note.clone(), note(if settings_exist { "ideSettings" } else { "ideNoSettings" }), note("ideExtension")],
            terminal_commands(&bin),
        ));
    }
    Ok(HostConnections {
        shim_directory: bin.to_string_lossy().into_owned(),
        shims,
        hosts,
    })
}
