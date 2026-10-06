use aam_protocol::{ApiError, NATIVE_DEFAULT_MODEL};
use std::ffi::{OsStr, OsString};

/// 막힌 옵션·하위 명령의 이름만 말한다. 값은 비밀일 수 있으므로 넣지 않는다.
fn named(code: &str, name: &str, why: &str, next: &str) -> ApiError {
    ApiError::new(
        code,
        format!(
            "관리 실행에서 사용할 수 없습니다: '{name}'. {why} {next} 입력 값은 출력하지 않았습니다."
        ),
    )
}

fn settings_blocked() -> ApiError {
    named(
        "AUTH_OVERRIDE_CONFLICT",
        "--settings",
        "인증·엔드포인트·모델·helper 설정을 담을 수 있습니다.",
        "호스트가 넣는 hook 설정만 허용됩니다.",
    )
}

/// `--settings` 검사의 기존 호출 이름. 다른 거부는 `named`를 쓴다.
fn conflict() -> ApiError {
    settings_blocked()
}

fn auth_named(name: &str, next: &str) -> ApiError {
    named(
        "AUTH_OVERRIDE_CONFLICT",
        name,
        "구독 인증·프로필·공급자·엔드포인트를 바꿀 수 있습니다.",
        next,
    )
}

fn unsupported_named(name: &str, why: &str, next: &str) -> ApiError {
    named("NATIVE_OPTION_UNSUPPORTED", name, why, next)
}

fn unreadable_arg() -> ApiError {
    ApiError::new(
        "NATIVE_OPTION_UNSUPPORTED",
        "확인할 수 없는 인수는 관리 실행에서 사용할 수 없습니다. 값은 출력하지 않았습니다.",
    )
}

/// 재개 옵션이 겹친 경우. launcher 테스트가 이 코드를 고정하므로 바꾸지 않는다.
fn duplicate_resume() -> ApiError {
    ApiError::new(
        "AUTH_OVERRIDE_CONFLICT",
        "관리 실행에서 사용할 수 없습니다: '--resume'. 재개 옵션을 겹쳐 지정했습니다. `--resume ID` 또는 `--continue` 중 하나만 사용하세요. 입력 값은 출력하지 않았습니다.",
    )
}

fn auth_option(tool: &str, name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    if lower.contains("api-key")
        || lower.contains("api_key")
        || lower.contains("apikey")
        || lower.contains("base-url")
        || lower.contains("base_url")
        || lower.contains("baseurl")
    {
        return true;
    }
    if matches!(name, "--profile" | "--mcp-config" | "--bare") {
        return true;
    }
    tool == "codex"
        && matches!(
            name,
            "-c" | "--config"
                | "--oss"
                | "--local-provider"
                | "--remote"
                | "--remote-auth-token-env"
                | "-p"
                | "--profile"
        )
}

fn model_option(tool: &str, name: &str) -> bool {
    name == "--model" || name == "--fallback-model" || (tool == "codex" && name == "-m")
}

fn blocked_session_flag(name: &str) -> Option<ApiError> {
    let (why, next) = match name {
        "--bg" | "--background" => (
            "백그라운드 세션은 계정 예약이 끝난 뒤에도 계속 돌아갑니다.",
            "앞단에서 실행하세요.",
        ),
        "--desktop" => (
            "데스크톱 앱으로 넘기면 계정 예약을 유지할 수 없습니다.",
            "터미널에서 실행하세요.",
        ),
        "--cloud" | "--environment" | "--teleport" | "--from-pr" => (
            "이 옵션은 Ojak이 계정을 매핑할 수 없는 다른 세션을 엽니다.",
            "관리 대화는 `aam continue`를 사용하세요.",
        ),
        _ => return None,
    };
    Some(unsupported_named(name, why, next))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum OptionKind {
    Flag,
    Value,
}

// 새 native 옵션은 인증·프로필·모델을 바꾸지 않는지 확인한 뒤에만 허용합니다.
fn option_kind(tool: &str, name: &str) -> Option<OptionKind> {
    let flag = OptionKind::Flag;
    let value = OptionKind::Value;
    match (tool, name) {
        (
            "claude",
            "--print"
            | "-p"
            | "--verbose"
            | "--debug"
            | "-d"
            | "--include-partial-messages"
            | "--replay-user-messages"
            | "--no-session-persistence"
            | "--strict-mcp-config"
            | "--disable-slash-commands"
            | "--dangerously-skip-permissions"
            | "--allow-dangerously-skip-permissions"
            | "--ide"
            | "--ax-screen-reader"
            | "--brief"
            | "--chrome"
            | "--no-chrome"
            | "--exclude-dynamic-system-prompt-sections"
            | "--fork-session"
            | "--forward-subagent-text"
            | "--include-hook-events"
            | "--safe-mode"
            | "--restricted"
            | "--prompt-suggestions"
            | "--remote-control"
            | "--worktree"
            | "-w",
        ) => Some(flag),
        (
            "claude",
            "--output-format"
            | "--input-format"
            | "--max-turns"
            | "--max-budget-usd"
            | "--permission-mode"
            | "--permission-prompts"
            | "--system-prompt"
            | "--append-system-prompt"
            | "--system-prompt-snapshot"
            | "--json-schema"
            | "--effort"
            | "--tools"
            | "--allowedTools"
            | "--allowed-tools"
            | "--disallowedTools"
            | "--disallowed-tools"
            | "--prefill"
            | "--name"
            | "--settings"
            | "--session-id"
            | "-n"
            | "--add-dir"
            | "--agent"
            | "--agents"
            | "--betas"
            | "--debug-file"
            | "--file"
            | "--plugin-dir"
            | "--plugin-url"
            | "--remote-control-session-name-prefix"
            | "--setting-sources",
        ) => Some(value),
        (
            "codex",
            "--json"
            | "--skip-git-repo-check"
            | "--ephemeral"
            | "--no-alt-screen"
            | "--full-auto"
            | "--search"
            | "--worktree"
            | "--strict-config"
            | "--dangerously-bypass-approvals-and-sandbox"
            | "--dangerously-bypass-hook-trust"
            | "--approve-for-me",
        ) => Some(flag),
        (
            "codex",
            "--sandbox"
            | "-s"
            | "--ask-for-approval"
            | "-a"
            | "--output-last-message"
            | "-o"
            | "--color"
            | "--add-dir"
            | "--image"
            | "-i"
            | "--cd"
            | "-C"
            | "--enable"
            | "--disable",
        ) => Some(value),
        _ => None,
    }
}

pub fn validate_native_args(tool: &str, args: &[OsString]) -> Result<(), ApiError> {
    let mut index = 0;
    let mut positional = false;
    let mut options_done = false;
    while index < args.len() {
        let arg = &args[index];
        if !options_done && arg == "--" {
            options_done = true;
            index += 1;
            continue;
        }
        // UTF-8이 아닌 인수는 옵션인지 판별할 수 없다. 위치 인수로 흘려보내면 `--settings=…\xff`처럼
        // 금지 옵션이 검사를 우회해 원본 CLI에 전달되므로 거부한다. 바이트는 출력하지 않는다.
        let text = arg.to_str().ok_or_else(unreadable_arg)?;
        if !options_done && text.starts_with('-') {
            let (name, inline) = text
                .split_once('=')
                .map_or((text, false), |(key, _)| (key, true));
            if let Some(error) = blocked_session_flag(name) {
                return Err(error);
            }
            if model_option(tool, name) {
                return Err(unsupported_named(
                    name,
                    "모델은 계정 배정에 쓰이므로 이 옵션으로 덮지 않습니다.",
                    "`claude --model 이름`, `codex --model 이름` 또는 `aam run --model 이름`으로 지정하세요.",
                ));
            }
            if auth_option(tool, name) {
                return Err(auth_named(name, "Ojak은 이 설정을 자동으로 바꾸지 않습니다."));
            }
            let takes_value = match option_kind(tool, name) {
                Some(OptionKind::Value) => true,
                Some(OptionKind::Flag) => false,
                None => {
                    return Err(unsupported_named(
                        name,
                        "아직 인증·프로필·모델에 영향이 없는지 확인하지 않은 옵션입니다.",
                        "확인된 옵션만 계정 배정 실행에 넘깁니다. 원본 CLI로 쓰려면 `aam deactivate` 후 다시 실행하세요.",
                    ));
                }
            };
            if !takes_value && inline {
                return Err(unsupported_named(
                    name,
                    "이 옵션은 값을 받지 않습니다.",
                    "값 없이 지정하세요.",
                ));
            }
            if takes_value && !inline {
                index += 1;
                if index >= args.len() {
                    return Err(ApiError::new(
                        "INVALID_ARGUMENT",
                        format!("'{name}' 뒤에 값이 필요합니다. 값은 출력하지 않았습니다."),
                    ));
                }
                // 옵션 값으로 다음 옵션을 삼켜 검사에서 누락시키지 않습니다. 그 토큰은 출력하지 않습니다.
                if args[index].to_str().is_some_and(|v| v.starts_with('-')) {
                    return Err(unsupported_named(
                        name,
                        "옵션 값으로 다른 옵션을 받지 않습니다.",
                        "값을 옵션 뒤에 바로 적으세요.",
                    ));
                }
            }
            if tool == "claude" && name == "--settings" {
                let value = if inline {
                    text.split_once('=').map(|(_, value)| value)
                } else {
                    args[index].to_str()
                }
                .ok_or_else(settings_blocked)?;
                validate_host_settings(value)?;
            }
        } else if !positional {
            if let Some(error) = subcommand_block(tool, text) {
                return Err(error);
            }
            positional = true;
        } else {
            positional = true;
        }
        index += 1;
    }
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
pub enum Resume {
    Session(String),
    Continue,
}

pub fn shim_resume(
    tool: &str,
    args: &[OsString],
) -> Result<(Option<Resume>, Vec<OsString>), ApiError> {
    let mut resume = None;
    let mut native = Vec::with_capacity(args.len());
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if arg == "--" {
            native.extend_from_slice(&args[index..]);
            break;
        }
        let text = arg.to_str().unwrap_or("");
        let (name, inline) = text
            .split_once('=')
            .map_or((text, None), |(a, b)| (a, Some(b)));
        if tool == "claude"
            && ((name.starts_with("-r") && name != "-r")
                || (name.starts_with("-c") && name != "-c"))
        {
            return Err(ApiError::new(
                "SESSION_REQUIRED",
                "축약된 재개 옵션은 대상을 구별할 수 없습니다. --resume ID 또는 --continue를 사용하세요.",
            ));
        }
        let selection = if tool == "claude" && matches!(name, "--continue" | "-c") {
            if inline.is_some() {
                return Err(unsupported_named(
                    "--continue",
                    "이 옵션은 값을 받지 않습니다.",
                    "`--continue`만 지정하세요.",
                ));
            }
            Some(Resume::Continue)
        } else if tool == "claude" && matches!(name, "--resume" | "-r") {
            let value = match inline {
                Some(value) => value,
                None => {
                    index += 1;
                    args.get(index).and_then(|value| value.to_str()).ok_or_else(|| {
                        ApiError::new("SESSION_REQUIRED", "관리 세션 ID가 필요합니다. 대화 선택기는 계정을 추측하므로 지원하지 않습니다.")
                    })?
                }
            };
            if value.is_empty() || value.starts_with('-') {
                return Err(unsupported_named(
                    "--resume",
                    "재개 대상이 비었거나 다른 옵션입니다.",
                    "`--resume ID`로 전체 ID를 지정하세요.",
                ));
            }
            Some(Resume::Session(value.to_owned()))
        } else {
            native.push(arg.clone());
            if inline.is_none() && takes_option_value(tool, name) {
                if let Some(value) = args.get(index + 1) {
                    index += 1;
                    native.push(value.clone());
                }
            }
            None
        };
        if let Some(selection) = selection {
            if resume.replace(selection).is_some() {
                return Err(duplicate_resume());
            }
        }
        index += 1;
    }
    Ok((resume, native))
}

pub fn shim_session_id(
    tool: &str,
    args: &[OsString],
) -> Result<(Option<String>, Vec<OsString>), ApiError> {
    let mut session = None;
    let mut native = Vec::with_capacity(args.len());
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if arg == "--" {
            native.extend_from_slice(&args[index..]);
            break;
        }
        let text = arg.to_str().unwrap_or("");
        let (name, inline) = text
            .split_once('=')
            .map_or((text, None), |(name, value)| (name, Some(value)));
        if tool == "claude" && name == "--session-id" {
            let value = match inline {
                Some(value) => value,
                None => {
                    index += 1;
                    args.get(index).and_then(|value| value.to_str()).ok_or_else(|| {
                        unsupported_named(
                            "--session-id",
                            "세션 ID가 없습니다.",
                            "UUID를 이어서 지정하세요.",
                        )
                    })?
                }
            };
            if uuid::Uuid::parse_str(value).is_err() || session.replace(value.to_owned()).is_some()
            {
                return Err(ApiError::new(
                    "SESSION_MAPPING_INVALID",
                    "새 Claude 세션 ID는 중복 없는 UUID여야 합니다.",
                ));
            }
        } else {
            native.push(arg.clone());
            if inline.is_none() && option_kind(tool, name) == Some(OptionKind::Value) {
                index += 1;
                native.push(args.get(index).ok_or_else(|| {
                    unsupported_named(name, "옵션 값이 없습니다.", "값을 이어서 적으세요.")
                })?.clone());
            }
        }
        index += 1;
    }
    Ok((session, native))
}

// 호스트가 넘기는 inline hook 객체만 허용합니다. 파일·env·인증·모델 설정은 받지 않습니다.
fn validate_host_settings(text: &str) -> Result<(), ApiError> {
    if text.len() > 64 * 1024 {
        return Err(conflict());
    }
    let value: serde_json::Value = serde_json::from_str(text).map_err(|_| conflict())?;
    let root = value.as_object().ok_or_else(conflict)?;
    if root
        .keys()
        .any(|key| !matches!(key.as_str(), "hooks" | "preferredNotifChannel"))
        || root
            .get("preferredNotifChannel")
            .is_some_and(|value| value.as_str() != Some("notifications_disabled"))
        || !root.contains_key("hooks")
    {
        return Err(conflict());
    }
    let events = root["hooks"].as_object().ok_or_else(conflict)?;
    for (event, groups) in events {
        if !matches!(
            event.as_str(),
            "SessionStart"
                | "SessionEnd"
                | "Stop"
                | "Notification"
                | "UserPromptSubmit"
                | "PreToolUse"
                | "PostToolUse"
                | "PostToolUseFailure"
                | "PermissionRequest"
                | "SubagentStart"
                | "SubagentStop"
        ) {
            return Err(conflict());
        }
        for group in groups.as_array().ok_or_else(conflict)? {
            let group = group.as_object().ok_or_else(conflict)?;
            if group
                .keys()
                .any(|key| !matches!(key.as_str(), "matcher" | "hooks"))
                || group.get("matcher").is_some_and(|value| !value.is_string())
            {
                return Err(conflict());
            }
            for hook in group
                .get("hooks")
                .and_then(serde_json::Value::as_array)
                .ok_or_else(conflict)?
            {
                let hook = hook.as_object().ok_or_else(conflict)?;
                if hook
                    .keys()
                    .any(|key| !matches!(key.as_str(), "type" | "command" | "timeout" | "async"))
                    || hook.get("type").and_then(serde_json::Value::as_str) != Some("command")
                    || hook
                        .get("command")
                        .and_then(serde_json::Value::as_str)
                        .is_none_or(|command| !known_host_hook(command))
                    || hook.get("timeout").is_some_and(|value| {
                        value.as_f64().is_none_or(|timeout| {
                            !timeout.is_finite() || timeout <= 0.0 || timeout > 600.0
                        })
                    })
                    || hook.get("async").is_some_and(|value| !value.is_boolean())
                {
                    return Err(conflict());
                }
            }
        }
    }
    Ok(())
}

fn known_host_hook(command: &str) -> bool {
    let Some(command) = command.strip_prefix("\"${CMUX_CLAUDE_HOOK_CMUX_BIN:-cmux}\" hooks ")
    else {
        return false;
    };
    matches!(
        command,
        "feed --source claude"
            | "claude session-start"
            | "claude stop"
            | "claude auto-name"
            | "claude session-end"
            | "claude notification"
            | "claude prompt-submit"
            | "claude cron-create-guard"
            | "claude pre-tool-use"
            | "claude push-notification"
    )
}

fn takes_option_value(tool: &str, name: &str) -> bool {
    option_kind(tool, name) == Some(OptionKind::Value)
        || name == "--model"
        || (tool == "codex" && name == "-m")
}

fn auth_command(name: &str) -> ApiError {
    named(
        "AUTH_OVERRIDE_CONFLICT",
        name,
        "이 명령은 계정 로그인을 바꿉니다.",
        "계정 추가는 앱의 연결 > 계정 추가 또는 `aam account login`을 사용하세요.",
    )
}

fn config_command(name: &str) -> ApiError {
    named(
        "AUTH_OVERRIDE_CONFLICT",
        name,
        "설정 명령은 인증·모델·엔드포인트를 바꿀 수 있어 키를 가리지 않고 막습니다.",
        "설치 상태는 `claude doctor` 또는 `codex doctor`로 확인하세요.",
    )
}

fn session_command(name: &str) -> ApiError {
    named(
        "NATIVE_OPTION_UNSUPPORTED",
        name,
        "대화 선택기는 어느 계정인지 알 수 없습니다.",
        "관리 대화는 `aam continue`를 사용하세요.",
    )
}

fn use_tool_command(tool: &str, name: &str) -> ApiError {
    named(
        "NATIVE_OPTION_UNSUPPORTED",
        name,
        "이 명령은 모델 세션을 시작하지 않아 계정을 배정하지 않습니다.",
        &format!("`{tool} {name}`으로 실행하세요."),
    )
}

fn unsupported_command(name: &str) -> ApiError {
    named(
        "NATIVE_OPTION_UNSUPPORTED",
        name,
        "모델 세션이 아니거나 계정 예약을 유지할 수 없는 명령입니다.",
        "대화는 옵션 없이 도구 이름으로 시작하세요.",
    )
}

/// 인증을 바꾸는 명령은 `AUTH_OVERRIDE_CONFLICT`, 그 외 관리 실행에 올리면 안 되는 명령은
/// `NATIVE_OPTION_UNSUPPORTED`. Codex `exec`만 비대화형 관리 실행으로 허용합니다.
fn passthrough_name(tool: &str, name: &str) -> bool {
    matches!(
        (tool, name),
        (
            "claude",
            "mcp" | "plugin" | "plugins" | "doctor" | "help" | "install" | "update" | "upgrade"
        ) | (
            "codex",
            "mcp" | "plugin" | "doctor" | "completion" | "features" | "help" | "update"
        )
    )
}

fn subcommand_block(tool: &str, name: &str) -> Option<ApiError> {
    if tool == "codex" && name == "exec" {
        return None;
    }
    if passthrough_name(tool, name) {
        return Some(use_tool_command(tool, name));
    }
    Some(match name {
        "auth" | "login" | "logout" | "setup-token" => auth_command(name),
        "config" | "settings" | "profile" | "profiles" | "gateway" | "import" => {
            config_command(name)
        }
        "resume" | "continue" | "fork" => session_command(name),
        "exec" | "run" | "app-server" | "mcp-server" | "server" | "remote-control"
        | "uninstall" | "debug" | "agents" | "attach" | "auto-mode" | "logs" | "project"
        | "respawn" | "rm" | "stop" | "kill" | "ultrareview" | "cloud" | "mcp" | "plugin"
        | "plugins" | "doctor" | "completion" | "features" | "help" | "update" | "upgrade"
        | "install" => unsupported_command(name),
        _ => return None,
    })
}

pub fn shim_model(tool: &str, args: &[OsString]) -> Result<(String, Vec<OsString>), ApiError> {
    let mut model = None;
    let mut native = Vec::with_capacity(args.len());
    let mut index = 0;
    let mut options_done = false;
    while index < args.len() {
        let arg = &args[index];
        if arg == "--" {
            options_done = true;
        }
        let model_flag = !options_done && (arg == "--model" || (tool == "codex" && arg == "-m"));
        let inline_model = !options_done
            && arg.to_str().is_some_and(|text| {
                text.starts_with("--model=") || (tool == "codex" && text.starts_with("-m="))
            });
        if model_flag {
            index += 1;
            let value = args
                .get(index)
                .and_then(|v| v.to_str())
                .filter(|s| !s.is_empty() && !s.starts_with('-'))
                .ok_or_else(|| {
                    ApiError::new(
                        "MODEL_REQUIRED",
                        "--model 뒤에 모델 이름이 필요합니다. 옵션을 생략하면 공식 CLI의 기본 설정을 유지합니다. 값은 출력하지 않았습니다.",
                    )
                })?;
            if model.replace(value.to_owned()).is_some() {
                return Err(unsupported_named(
                    "--model",
                    "모델을 두 번 지정했습니다.",
                    "`claude --model 이름`, `codex --model 이름` 또는 `aam run --model 이름`으로 한 번만 지정하세요.",
                ));
            }
        } else if inline_model {
            let text = arg.to_str().unwrap();
            let value = text
                .strip_prefix("--model=")
                .or_else(|| text.strip_prefix("-m="))
                .unwrap_or("");
            if value.is_empty() {
                return Err(ApiError::new(
                    "MODEL_REQUIRED",
                    "--model 뒤에 모델 이름이 필요합니다. 옵션을 생략하면 공식 CLI의 기본 설정을 유지합니다. 값은 출력하지 않았습니다.",
                ));
            }
            if model.replace(value.to_owned()).is_some() {
                return Err(unsupported_named(
                    "--model",
                    "모델을 두 번 지정했습니다.",
                    "`claude --model 이름`, `codex --model 이름` 또는 `aam run --model 이름`으로 한 번만 지정하세요.",
                ));
            }
        } else {
            native.push(arg.clone());
            if !options_done && arg.to_str().is_some_and(|name| takes_option_value(tool, name)) {
                index += 1;
                native.push(
                    args.get(index)
                        .ok_or_else(|| {
                            let name = arg.to_str().unwrap_or("옵션");
                            unsupported_named(name, "옵션 값이 없습니다.", "값을 이어서 적으세요.")
                        })?
                        .clone(),
                );
            }
        }
        index += 1;
    }
    Ok((model.unwrap_or_else(|| NATIVE_DEFAULT_MODEL.into()), native))
}

pub fn tool_from_argv0(value: &OsStr) -> Option<&'static str> {
    let path = std::path::Path::new(value);
    // Windows shim은 `claude.exe`처럼 확장자가 붙은 복사본이다.
    let name = if cfg!(windows) { path.file_stem()? } else { path.file_name()? };
    // Windows 파일 이름은 대소문자를 구분하지 않는다. macOS 동작은 그대로 둔다.
    let name = name.to_str()?;
    let name = if cfg!(windows) { name.to_ascii_lowercase() } else { name.to_owned() };
    match name.as_str() {
        "claude" => Some("claude"),
        "codex" => Some("codex"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn args(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    #[test]
    fn native_resume_extracts_identity_without_touching_prompt_values() {
        let (resume, forwarded) = shim_resume(
            "claude",
            &args(&[
                "-r",
                "native-id",
                "--system-prompt",
                "--continue",
                "--",
                "-r",
            ]),
        )
        .unwrap();
        assert_eq!(resume, Some(Resume::Session("native-id".into())));
        assert_eq!(
            forwarded,
            args(&["--system-prompt", "--continue", "--", "-r"])
        );
        assert!(shim_resume("claude", &args(&["-c", "-r", "other"])).is_err());
        assert!(shim_resume("claude", &args(&["--resume"])).is_err());
    }

    #[test]
    fn cmux_session_id_is_preserved_and_invalid_identity_is_rejected() {
        let id = "12345678-1234-4234-8234-123456789abc";
        let (session, forwarded) =
            shim_session_id("claude", &args(&["--session-id", id, "--print", "hello"])).unwrap();
        assert_eq!(session.as_deref(), Some(id));
        assert_eq!(forwarded, args(&["--print", "hello"]));
        assert!(shim_session_id("claude", &args(&["--session-id", "../transcript"])).is_err());
        assert!(
            shim_session_id("claude", &args(&["--session-id", id, "--session-id", id])).is_err()
        );
    }

    #[test]
    fn host_hooks_allow_metadata_but_not_auth_model_or_unknown_settings() {
        let settings = r#"{"preferredNotifChannel":"notifications_disabled","hooks":{"Stop":[{"matcher":"","hooks":[{"type":"command","command":"\"${CMUX_CLAUDE_HOOK_CMUX_BIN:-cmux}\" hooks claude stop","timeout":10,"async":true}]}]}}"#;
        assert!(validate_native_args("claude", &args(&["--settings", settings])).is_ok());
        // UTF-8이 아닌 인수는 옵션 검사를 우회할 수 있으므로 통째로 거부한다.
        #[cfg(unix)]
        let raw = {
            use std::os::unix::ffi::OsStringExt;
            OsString::from_vec(b"--settings=/tmp/x\xff".to_vec())
        };
        #[cfg(windows)]
        let raw = {
            // 짝 없는 서로게이트는 UTF-8로 바꿀 수 없는 Windows 인수다.
            use std::os::windows::ffi::OsStringExt;
            let mut wide: Vec<u16> = "--settings=/tmp/x".encode_utf16().collect();
            wide.push(0xD800);
            OsString::from_wide(&wide)
        };
        assert!(validate_native_args("claude", &[raw]).is_err());
        for settings in [
            r#"{"hooks":{},"env":{"ANTHROPIC_API_KEY":"secret"}}"#,
            r#"{"hooks":{},"model":"other"}"#,
            r#"{"hooks":{},"processWrapper":"/other"}"#,
            r#"{"hooks":{"Stop":[{"hooks":[{"type":"prompt","prompt":"replace"}]}]}}"#,
            r#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"claude auth login"}]}]}}"#,
            r#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"echo","env":{}}]}]}}"#,
            "settings.json",
        ] {
            assert!(validate_native_args("claude", &args(&["--settings", settings])).is_err());
        }
    }

    #[test]
    fn blocks_identity_overrides_and_native_resume() {
        for values in [
            vec!["--api-key=secret"],
            vec!["--settings", "private.json"],
            vec!["-cprovider=x"],
            vec!["--profile", "other"],
            vec!["--resume", "native-id"],
            vec!["--model", "other"],
            vec!["auth", "login"],
            vec!["--print", "--output-format", "--api-key=secret"],
        ] {
            assert!(validate_native_args("claude", &args(&values)).is_err());
        }
    }

    #[test]
    fn codex_non_interactive_run_is_managed_but_auth_subcommands_are_not() {
        // 비대화형 실행은 Claude의 --print와 같은 실행 모드이므로 관리 실행으로 허용합니다.
        assert!(validate_native_args(
            "codex",
            &args(&["exec", "--json", "--skip-git-repo-check", "요청 내용"])
        )
        .is_ok());
        for values in [
            vec!["login"],
            vec!["setup-token"],
            vec!["app-server"],
            vec!["config"],
            vec!["logout"],
            vec!["--api-key=secret"],
            vec!["--model", "other"],
        ] {
            assert!(
                validate_native_args("codex", &args(&values)).is_err(),
                "{values:?}"
            );
        }
        // 다른 도구에서는 exec를 계속 막습니다.
        assert!(validate_native_args("claude", &args(&["exec"])).is_err());
    }

    #[test]
    fn preserves_prompt_and_pipe_mode_arguments() {
        let original = args(&[
            "--print",
            "--output-format=json",
            "--system-prompt",
            "use --profile only in examples",
            "prompt with spaces\nand newlines",
        ]);
        assert!(validate_native_args("claude", &original).is_ok());
        let mut shim = args(&["--model", "claude-opus-4-6"]);
        shim.extend(original.clone());
        let (model, forwarded) = shim_model("claude", &shim).unwrap();
        assert_eq!(model, "claude-opus-4-6");
        assert_eq!(forwarded, original);
    }

    #[test]
    fn model_in_prompt_is_not_a_launch_override() {
        let (model, forwarded) = shim_model(
            "claude",
            &args(&["--model=claude-sonnet-4-6", "--", "--model=prompt-text"]),
        )
        .unwrap();
        assert_eq!(model, "claude-sonnet-4-6");
        assert_eq!(forwarded, args(&["--", "--model=prompt-text"]));
        assert!(shim_model("claude", &args(&["--model=a", "--model=b"])).is_err());
        let (_, forwarded) = shim_model(
            "claude",
            &args(&["--model=a", "--system-prompt", "--model=literal-prompt"]),
        )
        .unwrap();
        assert_eq!(
            forwarded,
            args(&["--system-prompt", "--model=literal-prompt"])
        );
    }

    #[test]
    fn harmless_flags_pass_and_blocks_name_the_option_without_its_value() {
        assert!(validate_native_args(
            "claude",
            &args(&["--add-dir", "/tmp/work", "--debug", "--verbose"])
        )
        .is_ok());
        assert!(validate_native_args("codex", &args(&["--add-dir", "/tmp/work", "--search"])).is_ok());
        let secret = "super-secret-value";
        let settings = format!(
            r#"{{"apiKeyHelper":"{secret}","baseUrl":"https://example.invalid"}}"#
        );
        let error = validate_native_args("claude", &args(&["--settings", &settings])).unwrap_err();
        assert_eq!(error.code, "AUTH_OVERRIDE_CONFLICT");
        assert!(error.message.contains("--settings"), "{}", error.message);
        assert!(!error.message.contains(secret), "{}", error.message);
        assert!(!error.message.contains("example.invalid"), "{}", error.message);
        let error = validate_native_args("claude", &args(&["--base-url", secret])).unwrap_err();
        assert_eq!(error.code, "AUTH_OVERRIDE_CONFLICT");
        assert!(error.message.contains("--base-url"), "{}", error.message);
        assert!(!error.message.contains(secret), "{}", error.message);
        let (model, forwarded) = shim_model("codex", &args(&["-m", "gpt-5", "--search"])).unwrap();
        assert_eq!(model, "gpt-5");
        assert_eq!(forwarded, args(&["--search"]));
        let error = shim_model("codex", &args(&["-m", "secret-model", "-m", "other"])).unwrap_err();
        assert!(error.message.contains("--model"), "{}", error.message);
        assert!(!error.message.contains("secret-model"), "{}", error.message);
        let error = validate_native_args("claude", &args(&["auth", "login"])).unwrap_err();
        assert_eq!(error.code, "AUTH_OVERRIDE_CONFLICT");
        assert!(error.message.contains("auth"), "{}", error.message);
        assert!(error.message.contains("aam account login"), "{}", error.message);
        let error = validate_native_args("codex", &args(&["login"])).unwrap_err();
        assert_eq!(error.code, "AUTH_OVERRIDE_CONFLICT");
        assert!(error.message.contains("login"), "{}", error.message);
        let error = validate_native_args("claude", &args(&["--not-a-real-flag"])).unwrap_err();
        assert_eq!(error.code, "NATIVE_OPTION_UNSUPPORTED");
        assert!(error.message.contains("--not-a-real-flag"), "{}", error.message);
        let error = validate_native_args("codex", &args(&["resume"])).unwrap_err();
        assert_eq!(error.code, "NATIVE_OPTION_UNSUPPORTED");
        assert!(error.message.contains("resume"), "{}", error.message);
        assert!(error.message.contains("aam continue"), "{}", error.message);
    }

}
