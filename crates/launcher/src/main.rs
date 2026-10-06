mod deactivate;
mod omp_observer;
#[cfg(windows)]
mod omp_observer_writer;
mod omp_bridge;
mod omp_broker;
mod omp_extension;

use aam_launcher::{arguments, current_cwd, install, read_snapshot, supervisor};
use aam_protocol::{call, ApiError, LaunchIntent, Paths, Snapshot, NATIVE_DEFAULT_MODEL};
use clap::{Args, CommandFactory, Parser, Subcommand};
use serde_json::{json, Value};
use std::{ffi::OsString, path::PathBuf};
#[cfg(windows)]
use std::path::Path;

#[derive(Parser)]
#[command(
    name = "aam",
    version,
    about = "AI 계정 상태와 안전한 native 실행을 관리합니다.",
    after_help = "native 인수는 -- 뒤에 전달합니다. 인증·모델·프로필 변경 옵션은 차단하며 원본 로그인과 shell 설정을 자동으로 바꾸지 않습니다."
)]
struct Cli {
    #[command(subcommand)]
    command: Action,
}
#[derive(Subcommand)]
enum Action {
    #[cfg(windows)]
    #[command(hide = true)]
    Installer {
        action: String,
        #[arg(long)]
        directory: PathBuf,
    },
    #[command(about = "실제 계정·할당·세션 상태를 조회합니다.")]
    Status {
        #[arg(long)]
        json: bool,
    },
    #[command(about = "설치 도구와 검증되지 않은 지원 기능을 확인합니다.")]
    Doctor,
    #[command(about = "추론 없이 계정과 사용량을 새로 조회합니다.")]
    Refresh,
    #[command(about = "예약하지 않고 계정 선택 이유를 확인합니다.")]
    Explain(Explain),
    #[command(about = "선택 계정을 최종 확인한 뒤 native CLI를 실행합니다.")]
    Run(Run),
    #[command(about = "한도를 다 쓴 Claude Code·Codex 대화를 다른 계정에서 이어 엽니다(대화 기록만 옮기고 로그인은 바꾸지 않음).")]
    Continue {
        #[arg(long, help = "claude 또는 codex. 없으면 현재 폴더에서 더 최근에 쓴 도구")]
        tool: Option<String>,
        #[arg(long, help = "이어 갈 관리 세션 ID. 없으면 현재 폴더에서 이 도구의 마지막 대화")]
        session: Option<String>,
        #[arg(long, help = "이어 받을 계정 ID. 없으면 원래 계정을 뺀 자동 배정")]
        account: Option<String>,
        #[arg(last = true, help = "도구에 그대로 넘길 인수(예: -- -p \"질문\", Codex는 -- exec \"질문\")")]
        native_args: Vec<std::ffi::OsString>,
    },
    #[command(about = "기존 프로필을 연결하거나 명시적인 native 로그인을 시작합니다.")]
    Account {
        #[command(subcommand)]
        action: AccountAction,
    },
    #[command(about = "사용자 단위 LaunchAgent를 명시적으로 설치하거나 제거합니다.")]
    Service {
        #[command(subcommand)]
        action: ServiceAction,
    },
    #[command(about = "실행 가능한 도구만 원본 CLI를 보존하는 관리형 shim에 연결합니다.")]
    Integration {
        #[command(subcommand)]
        action: IntegrationAction,
    },
    #[command(about = "원본 OMP를 auth broker에 연결하거나 연결을 해제합니다.")]
    OmpBroker {
        #[command(subcommand)]
        action: OmpBrokerAction,
    },
    #[command(about = "omp의 Ojak 공급자를 자동으로 로그인하거나 연결을 해제합니다. 새 세션에서 /model로 ojak-* 모델을 고릅니다.")]
    OmpBridge {
        #[command(subcommand)]
        action: OmpBridgeAction,
    },
    #[command(about = "OMP의 읽기 전용 계정 관측 확장을 설치하거나 제거합니다.")]
    OmpObserver {
        #[command(subcommand)]
        action: ObserverAction,
    },
    #[command(about = "외부에서 시작한 대화를 확인된 소유 계정으로 관리에 인계합니다.")]
    Takeover {
        #[command(subcommand)]
        action: TakeoverAction,
    },
    #[command(about = "처음 쓰는 데 필요한 서비스·명령 연결·터미널 PATH 중 빠진 것만 설치하고 준비 상태를 JSON으로 출력합니다.")]
    Setup {
        #[arg(long, conflicts_with = "status", help = "설치하지 않고 새 로그인 셸의 명령 연결까지 확인합니다.")]
        check: bool,
        #[arg(long, conflicts_with = "with_omp", help = "사용자 셸을 실행하지 않고 설정 상태만 조회합니다.")]
        status: bool,
        #[arg(long, conflicts_with = "check", help = "기존 안전 설치 경로로 omp 연결도 함께 준비합니다.")]
        with_omp: bool,
    },
    #[command(about = "zsh의 앱 전용 PATH 블록을 명시적으로 설치하거나 제거합니다.")]
    Shell {
        #[command(subcommand)]
        action: ShellAction,
    },
    #[command(about = "Ojak 연결을 모두 풀어 설치 전 형태로 되돌립니다. 계정·프로필·로그는 보존합니다.")]
    Deactivate {
        #[arg(long, help = "바꾸지 않고 되돌릴 단계만 보여 줍니다.")]
        dry_run: bool,
    },
}
#[derive(Args)]
#[command(
    after_help = "관리 실행: aam run claude|codex [--account <계정 ID>] [--model <모델>] [--cwd <프로젝트>] [-- <native 인수>]\n계정을 생략하면 관리 정책에 따라 시작할 때 한 번 선택하고 실행 중에는 변경하지 않습니다. omp는 Ojak이 실행하지 않으며, 새 omp 세션에서 /model로 ojak-* 모델을 고르면 Ojak이 계정을 배정합니다."
)]
struct Run {
    tool: String,
    #[arg(long, default_value = NATIVE_DEFAULT_MODEL)]
    model: String,
    #[arg(long)]
    cwd: Option<PathBuf>,
    #[arg(long)]
    account: Option<String>,
    #[arg(long)]
    resume_session: Option<String>,
    #[arg(last = true, allow_hyphen_values = true)]
    native_args: Vec<OsString>,
}
#[derive(Args)]
struct Explain {
    #[arg(long)]
    tool: String,
    #[arg(long, default_value = NATIVE_DEFAULT_MODEL)]
    model: String,
    #[arg(long)]
    cwd: Option<PathBuf>,
    #[arg(long)]
    account: Option<String>,
}
#[derive(Subcommand)]
enum AccountAction {
    #[command(about = "기존 공식 CLI 프로필을 복제하지 않고 연결합니다.")]
    Add {
        #[arg(long)]
        tool: String,
        #[arg(long)]
        label: String,
        #[arg(long)]
        profile: PathBuf,
    },
    #[command(about = "분리된 프로필에서 공식 로그인을 실행합니다. --account가 있으면 그 프로필에서 다시 로그인합니다.")]
    Login {
        #[arg(long)]
        tool: String,
        #[arg(long, required_unless_present = "account")]
        label: Option<String>,
        #[arg(long, conflicts_with_all = ["settings_digest", "fresh_settings", "label"])]
        account: Option<String>,
        #[arg(long, conflicts_with = "fresh_settings")]
        settings_digest: Option<String>,
        #[arg(
            long,
            required_unless_present_any = ["settings_digest", "account"],
            conflicts_with = "settings_digest"
        )]
        fresh_settings: bool,
    },
    #[command(about = "새 프로필에 가져올 수 있는 인증 없는 설정과 제외 항목을 미리 봅니다.")]
    SettingsPreview {
        #[arg(long)]
        tool: String,
    },
}
#[derive(Subcommand)]
enum ServiceAction {
    Install,
    Uninstall,
    Stop,
    Status,
}
#[derive(Subcommand)]
enum IntegrationAction {
    Install,
    Uninstall,
    /// 연결을 이미 설치했으면 새로 실행 가능해진 도구의 shim만 더한다. 설치한 적이 없으면 아무것도 하지 않는다.
    Extend {
        #[arg(long)]
        tool: String,
    },
}

#[derive(Subcommand)]
enum OmpBrokerAction {
    Status { #[arg(long)] json: bool },
    Connect,
    Disconnect,
}

#[derive(Subcommand)]
enum OmpBridgeAction {
    Status { #[arg(long)] json: bool },
    Connect,
    Disconnect,
}

#[derive(Subcommand)]
enum ObserverAction {
    Status,
    Install,
    Uninstall,
    #[cfg(windows)]
    #[command(hide = true)]
    Write,
}
#[derive(Subcommand)]
enum TakeoverAction {
    #[command(about = "등록된 인계와 인계 가능한 외부 대화를 확인합니다.")]
    List,
    #[command(
        about = "대화의 소유 계정을 확인해 인계합니다. Claude는 대화 UUID, OMP는 대화 파일 경로를 지정합니다."
    )]
    Adopt {
        tool: String,
        session: String,
    },
    #[command(about = "등록한 인계를 해제합니다. 이미 만들어진 관리 세션은 유지됩니다.")]
    Release {
        tool: String,
        session: String,
    },
}

#[derive(Subcommand)]
enum ShellAction {
    Install,
    Uninstall,
}

fn path_text(path: PathBuf) -> Result<String, ApiError> {
    path.into_os_string()
        .into_string()
        .map_err(|_| ApiError::new("INVALID_PATH", "경로는 UTF-8이어야 합니다."))
}
fn cwd(path: Option<PathBuf>) -> Result<String, ApiError> {
    path.map(path_text).unwrap_or_else(current_cwd)
}
fn print_json(value: &impl serde::Serialize) -> Result<(), ApiError> {
    println!(
        "{}",
        serde_json::to_string_pretty(value).map_err(|_| ApiError::new(
            "SERIALIZATION_FAILED",
            "출력 데이터를 생성하지 못했습니다."
        ))?
    );
    Ok(())
}
fn print_snapshot(snapshot: &Snapshot) {
    println!(
        "Ojak · 계정 {}개 · 자동 배정 {}",
        snapshot.accounts.len(),
        if snapshot.policy.automatic {
            "켜짐"
        } else {
            "꺼짐"
        }
    );
    for account in &snapshot.accounts {
        println!(
            "{}  {}  {}  {}",
            account.id,
            account.tool,
            account.label,
            if account.can_launch {
                "실행 가능"
            } else {
                "실행 차단"
            }
        );
        println!(
            "  인증: {} · 확인: {}",
            account.auth_status, account.verification
        );
        if let Some(reason) = &account.reason {
            println!("  사유: {reason}");
        }
        for bucket in &account.buckets {
            let usage = bucket
                .used_percent
                .map(|v| format!("{v:.1}%"))
                .unwrap_or_else(|| "알 수 없음".into());
            println!(
                "  {}: {} · {} · {}",
                bucket.label, usage, bucket.status, bucket.source
            );
        }
    }
    println!("관리 세션 {}개 (종료 기록 포함)", snapshot.sessions.len());
    for session in &snapshot.sessions {
        println!(
            "{}  {}  {}  {}",
            session.id, session.tool, session.account_id, session.state
        );
    }
}

fn setup_omp(paths: &Paths, install: bool) -> Value {
    let installed = (|| -> Result<(), ApiError> {
        if !install { return Ok(()); }
        // 연결돼 있어도 감독 작업이 최신이 아니면(예: Windows에서 창을 띄우던 이전 broker 작업) 다시 연결해 옮긴다.
        if !omp_broker::status(paths).is_ok_and(|status| status.connected && status.supervised) { omp_broker::connect(paths)?; }
        omp_bridge::connect(paths)?;
        let observer: Value = serde_json::from_str(&omp_observer::run("status").map_err(|_| ApiError::new("OBSERVER_STATUS_FAILED", "관측 확장을 확인하지 못했습니다."))?)
            .map_err(|_| ApiError::new("PROTOCOL_MISMATCH", "관측 확장 응답을 해석하지 못했습니다."))?;
        if observer["current"] != true {
            if observer["installed"] == true {
                omp_observer::run("uninstall").map_err(|_| ApiError::new("OBSERVER_CONFLICT", "관측 확장 소유권을 확인할 수 없어 교체하지 않았습니다."))?;
            }
            omp_observer::run("install").map_err(|_| ApiError::new("OBSERVER_CONFLICT", "관측 확장을 설치하지 못했습니다. 기존 파일과 소유권을 확인하세요."))?;
        }
        Ok(())
    })();
    let broker = omp_broker::status(paths);
    let bridge = omp_bridge::status(paths);
    let observer = omp_observer::run("status").ok().and_then(|value| serde_json::from_str::<Value>(&value).ok());
    // 코드만 보이면 이유를 알 수 없다. 앱이 만든 문장(비밀 없음)을 함께 보낸다.
    let describe = |error: &ApiError| format!("{} ({})", error.message, error.code);
    let error = installed.err().map(|error| describe(&error))
        .or_else(|| broker.as_ref().err().map(describe))
        .or_else(|| bridge.as_ref().err().map(describe))
        .or_else(|| observer.is_none().then(|| "관측 확장을 확인하지 못했습니다. (OBSERVER_STATUS_FAILED)".into()));
    json!({
        "broker": broker.is_ok_and(|status| status.connected),
        "bridge": bridge.is_ok_and(|status| status.connected && status.bridge["listening"] == true),
        "observer": observer.is_some_and(|status| status["current"] == true),
        "error": error,
        "futureLaunchesOnly": true
    })
}

/// 서비스를 끄기 전에 `aam deactivate`와 같은 순서로 omp를 푼다. broker 블록이 남은 채 서비스가 사라지면 omp 로그인이 깨진다.
#[cfg(windows)]
fn remove_installation(paths: &Paths, directory: &Path) -> Result<(), ApiError> {
    install::confirm_removal_target(paths, directory)?;
    deactivate::run(paths, false)?;
    install::installer(paths, "remove", directory)
}

fn execute(paths: &Paths, action: Action) -> Result<(), ApiError> {
    match action {
        #[cfg(windows)]
        Action::Installer { action, directory } if action == "remove" => remove_installation(paths, &directory)?,
        #[cfg(windows)]
        Action::Installer { action, directory } => install::installer(paths, &action, &directory)?,
        Action::Status { json } => {
            let snapshot = read_snapshot(paths)?;
            if json {
                print_json(&snapshot)?;
            } else {
                print_snapshot(&snapshot);
            }
        }
        Action::Doctor => {
            let snapshot = read_snapshot(paths)?;
            println!("{}", install::service_status(paths)?);
            for tool in snapshot.tools {
                println!(
                    "{} · {} · 격리: {}",
                    tool.name,
                    if tool.installed {
                        "설치됨"
                    } else {
                        "미설치"
                    },
                    tool.isolation
                );
                if let Some(path) = tool.binary_path {
                    println!("  바이너리: {path}");
                }
                if let Some(reason) = tool.reason {
                    println!("  {reason}");
                }
            }
            println!("관리형 shim을 거치지 않는 절대 경로·외부 GUI·다른 기기의 실행은 통제하지 못합니다.");
        }
        Action::Refresh => {
            print_json(&call(paths, "quota.refresh", json!({}))?)?;
        }
        Action::Explain(args) => {
            let intent = LaunchIntent {
                tool: args.tool,
                model: args.model,
                cwd: cwd(args.cwd)?,
                account_id: args.account,
                parent_session_id: None,
                resume_session_id: None,
                native_session_id: None,
                adopted: false,
                continue_elsewhere: false,
            };
            print_json(&call(paths, "route.explain", json!({"intent":intent}))?)?;
        }
        Action::Run(args) => {
            let intent = LaunchIntent {
                tool: args.tool,
                model: args.model,
                cwd: cwd(args.cwd)?,
                account_id: args.account,
                parent_session_id: None,
                resume_session_id: args.resume_session,
                native_session_id: None,
                adopted: false,
                continue_elsewhere: false,
            };
            let status = aam_launcher::run(paths, intent, args.native_args)?;
            supervisor::finish_with_status(status);
        }
        Action::Continue { tool, session, account, native_args } => {
            let status = aam_launcher::continue_elsewhere(paths, tool, session, account, native_args)?;
            supervisor::finish_with_status(status);
        }
        Action::Account {
            action:
                AccountAction::Add {
                    tool,
                    label,
                    profile,
                },
        } => {
            let profile = std::fs::canonicalize(profile).map_err(|_| {
                ApiError::new("INVALID_PROFILE", "프로필 폴더를 확인할 수 없습니다.")
            })?;
            let value: Value = call(
                paths,
                "account.register",
                json!({"tool":tool,"label":label,"profilePath":path_text(profile)?}),
            )?;
            print_json(&value)?;
            if value.get("canLaunch").and_then(Value::as_bool) == Some(true) {
                let _ = aam_launcher::extend_integration(paths, &tool);
            }
        }
        Action::Account {
            action:
                AccountAction::Login {
                    tool,
                    label,
                    settings_digest,
                    fresh_settings: _,
                    account,
                },
        } => {
            let status = if let Some(account) = account {
                aam_launcher::account_relogin(paths, &tool, &account)?
            } else {
                let label = label.ok_or_else(|| ApiError::new("LABEL_INVALID", "계정 이름을 입력해 주세요."))?;
                aam_launcher::account_login(paths, &tool, &label, settings_digest.as_deref())?
            };
            supervisor::finish_with_status(status);
        }
        Action::Account {
            action: AccountAction::SettingsPreview { tool },
        } => {
            print_json(&aam_adapters::preview_settings(&tool)?)?;
        }
        Action::Service { action } => {
            println!(
                "{}",
                match action {
                    ServiceAction::Install => install::service_install(paths)?,
                    ServiceAction::Uninstall => install::service_uninstall(paths)?,
                    ServiceAction::Stop => install::service_stop(paths)?,
                    ServiceAction::Status => install::service_status(paths)?,
                }
            );
        }
        Action::Integration { action } => {
            println!(
                "{}",
                match action {
                    IntegrationAction::Install => install::integration_install(paths)?,
                    IntegrationAction::Uninstall => install::integration_uninstall(paths)?,
                    IntegrationAction::Extend { tool } => {
                        if aam_launcher::extend_integration(paths, &tool)? {
                            format!("새 터미널의 `{tool}` 명령을 Ojak 관리 실행에 연결했습니다.")
                        } else {
                            String::new()
                        }
                    }
                }
            );
        }
        Action::OmpBroker { action } => {
            match action {
                OmpBrokerAction::Status { json } => {
                    let value = omp_broker::status(paths)?;
                    if json {
                        print_json(&value)?;
                    } else {
                        println!("OMP 계정 연결: {} · 계정 {}개 · 자동 복구 {} · 설정: {}", if value.connected { "사용 가능" } else { "연결 안 됨" }, value.account_count.unwrap_or(0), if value.supervised { "등록됨" } else { "미등록" }, value.config_path);
                    }
                }
                OmpBrokerAction::Connect => print_json(&omp_broker::connect(paths)?)?,
                OmpBrokerAction::Disconnect => print_json(&omp_broker::disconnect(paths)?)?,
            }
        }
        Action::OmpBridge { action } => match action {
            OmpBridgeAction::Status { json } => {
                let value = omp_bridge::status(paths)?;
                if json {
                    print_json(&value)?;
                } else {
                    let gateways = value.bridge.get("gateways").and_then(|v| v.as_array()).map_or(0, Vec::len);
                    let sessions = value.bridge.get("sessions").and_then(|v| v.as_array()).map_or(0, Vec::len);
                    println!(
                        "OMP 계정 브릿지: {} · 계정 gateway {gateways}개 · 고정 세션 {sessions}개 · omp 확장: {}",
                        if value.connected { "연결됨 (새 omp 세션에서 /model로 ojak-* 모델 선택)" } else { "연결 안 됨" },
                        if value.extension_installed { value.extension_path.as_str() } else { "미설치" }
                    );
                    if let Some(error) = value.bridge.get("error").and_then(|v| v.as_str()) {
                        println!("오류: {error}");
                    }
                }
            }
            OmpBridgeAction::Connect => print_json(&omp_bridge::connect(paths)?)?,
            OmpBridgeAction::Disconnect => print_json(&omp_bridge::disconnect(paths)?)?,
        },
        Action::OmpObserver { action } => {
            #[cfg(windows)]
            if matches!(action, ObserverAction::Write) {
                return omp_observer_writer::serve(paths).map_err(|_| ApiError::new("OBSERVER_WRITE_FAILED", "관측 파일을 안전하게 기록하지 못했습니다."));
            }
            let action = match action {
                ObserverAction::Status => "status",
                ObserverAction::Install => "install",
                ObserverAction::Uninstall => "uninstall",
                #[cfg(windows)]
                ObserverAction::Write => unreachable!(),
            };
            let result = omp_observer::run(action)
                .map_err(|error| ApiError::new("OMP_OBSERVER_INSTALLATION", error.to_string()))?;
            println!("{result}");
        }
        Action::Takeover { action } => match action {
            TakeoverAction::List => {
                let snapshot = read_snapshot(paths)?;
                print_json(&json!({
                    "takeovers": snapshot.takeovers,
                    // 관측에서 실제 대화 파일을 확정한 외부 세션만 인계 후보로 제시합니다.
                    "candidates": snapshot
                        .observed_sessions
                        .iter()
                        .filter(|observed| observed.native_session_id.is_some())
                        .map(|observed| json!({
                            "tool": observed.tool,
                            "nativeSessionId": observed.native_session_id,
                            "cwd": observed.cwd,
                            "host": observed.host,
                        }))
                        .collect::<Vec<_>>(),
                    "autoTakeover": snapshot.policy.auto_takeover,
                }))?;
            }
            TakeoverAction::Adopt { tool, session } => {
                print_json(&call(
                    paths,
                    "takeover.adopt",
                    json!({"tool":tool,"nativeSessionId":session}),
                )?)?;
            }
            TakeoverAction::Release { tool, session } => {
                print_json(&call(
                    paths,
                    "takeover.release",
                    json!({"tool":tool,"nativeSessionId":session}),
                )?)?;
            }
        },
        Action::Setup { check, status, with_omp } => {
            let prepared = if status {
                aam_launcher::setup::setup_status(paths)
            } else if check {
                aam_launcher::setup::setup_check(paths)
            } else {
                aam_launcher::setup::setup_install(paths)?
            };
            let mut value = json!(prepared);
            // `ready`는 서비스·명령 연결·계정 준비만 뜻한다. omp는 선택 기능이라 결과를 `omp`에 따로 담는다.
            if prepared.omp_detected || with_omp { value["omp"] = setup_omp(paths, with_omp); }
            print_json(&value)?;
        }
        Action::Shell { action } => {
            println!(
                "{}",
                match action {
                    ShellAction::Install => install::shell_install(paths)?,
                    ShellAction::Uninstall => install::shell_uninstall(paths)?,
                }
            );
        }
        Action::Deactivate { dry_run } => println!("{}", deactivate::run(paths, dry_run)?),
    }
    Ok(())
}

enum ManagementParse {
    ShortHelp,
    PrintedHelp(clap::Error),
    Run(Cli),
}

fn help_kind(error: &clap::Error) -> bool {
    matches!(
        error.kind(),
        clap::error::ErrorKind::DisplayHelp
            | clap::error::ErrorKind::DisplayVersion
            | clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
    )
}

/// shim(claude/codex)은 이 함수에 오지 않는다. 인수 없는 `aam`은 짧은 도움말이다.
fn parse_management(args: &[OsString]) -> Result<ManagementParse, ApiError> {
    if args.len() <= 1 {
        return Ok(ManagementParse::ShortHelp);
    }
    match Cli::try_parse_from(args.iter().cloned()) {
        Ok(cli) => Ok(ManagementParse::Run(cli)),
        Err(error) if help_kind(&error) => Ok(ManagementParse::PrintedHelp(error)),
        // clap 원문에는 잘못 전달한 값이 들어갈 수 있다. 종류와 사용법 틀만 알리고 값은 넣지 않는다.
        Err(error) => Err(format_usage_error(&error)),
    }
}

fn format_usage_error(error: &clap::Error) -> ApiError {
    use clap::error::ErrorKind;
    let problem = match error.kind() {
        ErrorKind::MissingRequiredArgument => "필수 인수가 빠졌습니다.",
        ErrorKind::MissingSubcommand => "하위 명령이 빠졌습니다.",
        ErrorKind::InvalidSubcommand => "알 수 없는 하위 명령입니다.",
        ErrorKind::UnknownArgument => "알 수 없는 옵션 또는 인수입니다.",
        ErrorKind::InvalidValue | ErrorKind::ValueValidation => "인수 값이 올바르지 않습니다.",
        ErrorKind::TooManyValues => "인수 값이 너무 많습니다.",
        ErrorKind::TooFewValues => "인수 값이 부족합니다.",
        ErrorKind::WrongNumberOfValues => "인수 개수가 맞지 않습니다.",
        ErrorKind::ArgumentConflict => "함께 쓸 수 없는 옵션입니다.",
        ErrorKind::NoEquals => "이 옵션은 등호(=)로 값을 적어야 합니다.",
        ErrorKind::InvalidUtf8 => "인수에 올바른 문자가 아닌 값이 있습니다.",
        _ => "명령 형식이 올바르지 않습니다.",
    };
    let mut message = problem.to_string();
    if matches!(
        error.kind(),
        ErrorKind::MissingRequiredArgument | ErrorKind::TooFewValues | ErrorKind::WrongNumberOfValues
    ) {
        if let Some(names) = defined_arg_names(error) {
            message.push_str(&format!(" 필요한 항목: {names}."));
        }
    }
    if let Some(usage) = usage_hint(error) {
        message.push_str(" 사용법: ");
        message.push_str(&usage);
        message.push('.');
    }
    message.push_str(" 자세한 형식은 aam --help 또는 해당 명령의 --help를 확인하세요. 입력 값은 출력하지 않았습니다.");
    ApiError::new("INVALID_ARGUMENT", message)
}

fn defined_arg_names(error: &clap::Error) -> Option<String> {
    use clap::error::{ContextKind, ContextValue};
    let items = match error.get(ContextKind::InvalidArg)? {
        ContextValue::String(item) => vec![item.as_str()],
        ContextValue::Strings(items) => items.iter().map(String::as_str).collect(),
        _ => return None,
    };
    if items.is_empty() || items.iter().any(|item| !is_defined_arg(item)) {
        return None;
    }
    Some(items.join(", "))
}

fn is_defined_arg(item: &str) -> bool {
    !item.is_empty()
        && item.len() <= 80
        && item.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '<' | '>' | ' ' | '[' | ']' | '.'))
}

fn usage_hint(error: &clap::Error) -> Option<String> {
    use clap::error::{ContextKind, ContextValue};
    let ContextValue::StyledStr(usage) = error.get(ContextKind::Usage)? else {
        return None;
    };
    let text = plain_text(&usage.to_string());
    let text = text.trim().trim_start_matches("Usage:").trim();
    if text.is_empty() || text.len() > 400 || text.chars().any(char::is_control) {
        return None;
    }
    Some(text.to_owned())
}

fn plain_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for next in chars.by_ref() {
                    if next.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
            continue;
        }
        out.push(ch);
    }
    out
}

fn main() {
    // `aam status | head`처럼 읽는 쪽이 먼저 닫혀도 panic하지 않고 조용히 끝나게 기본 SIGPIPE 동작을 되살린다.
    // shim 모드의 native 자식은 이 설정을 물려받아 원래 CLI와 같은 파이프 동작을 한다.
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    // 안내 문구는 UTF-8로 쓴다. Windows 콘솔 기본 코드 페이지(CP949 등)로 읽으면 한글이 깨진다.
    #[cfg(windows)]
    unsafe {
        windows_sys::Win32::System::Console::SetConsoleOutputCP(65001);
    }
    let result = (|| {
        let paths = Paths::discover().map_err(|_| {
            ApiError::new("PATHS_UNAVAILABLE", "앱 관리 경로를 확인하지 못했습니다.")
        })?;
        let args: Vec<OsString> = std::env::args_os().collect();
        if args
            .get(1)
            .is_some_and(|arg| arg == "--claude-process-wrapper")
        {
            return aam_launcher::claude_process_wrapper(&paths, &args[2..]);
        }
        if args.get(1).is_some_and(|arg| arg == "--claude-native-exec") {
            return aam_launcher::claude_native_exec(&paths, &args[2..]);
        }
        // Windows shim(`claude.cmd`)은 `aam.exe --shim claude ...`로 부른다. Unix shim은 symlink라 argv0 이름으로 고른다.
        let shim = if args.get(1).is_some_and(|arg| arg == "--shim") {
            let tool = args.get(2).and_then(|arg| arg.to_str()).and_then(|name| ["claude", "codex"].into_iter().find(|tool| *tool == name))
                .ok_or_else(|| ApiError::new("INVALID_ARGUMENT", "알 수 없는 shim 도구입니다."))?;
            Some((tool, 3))
        } else {
            args.first().and_then(|arg| arguments::tool_from_argv0(arg)).map(|tool| (tool, 1))
        };
        if let Some((tool, skip)) = shim {
            let args = &args[skip - 1..];
            if let Some(status) = aam_adapters::inspect_cli(&paths, tool, &args[1..]) {
                supervisor::finish_with_status(status?);
            }
            let status = aam_launcher::run_cli(&paths, tool, args[1..].to_vec())?;
            supervisor::finish_with_status(status);
        }
        let cli = match parse_management(&args)? {
            ManagementParse::ShortHelp => {
                let mut command = Cli::command();
                let _ = command.print_help();
                println!();
                return Ok(());
            }
            ManagementParse::PrintedHelp(error) => {
                let _ = error.print();
                return Ok(());
            }
            ManagementParse::Run(cli) => cli,
        };
        execute(&paths, cli.command)
    })();
    if let Err(error) = result {
        eprintln!("aam: {} ({})", error.message, error.code);
        // 앱이 새 콘솔 창으로 연 실행은 끝나자마자 창이 닫혀 오류를 읽을 수 없다. 그 경우에만 Enter를 기다린다.
        if std::env::var_os("AAM_HOLD_ON_ERROR").is_some() {
            eprintln!("Enter를 누르면 창을 닫습니다.");
            let mut line = String::new();
            let _ = std::io::stdin().read_line(&mut line);
        }
        std::process::exit(1);
    }
}

#[cfg(test)]
mod usage_tests {
    use super::*;

    #[test]
    fn bare_aam_is_short_help() {
        let parsed = parse_management(&[OsString::from("aam")]).unwrap();
        assert!(matches!(parsed, ManagementParse::ShortHelp));
    }

    #[test]
    fn help_flag_is_not_a_usage_error() {
        let parsed = parse_management(&[OsString::from("aam"), OsString::from("--help")]).unwrap();
        assert!(matches!(parsed, ManagementParse::PrintedHelp(_)));
    }

    #[test]
    fn missing_argument_names_the_problem_and_usage() {
        let Err(error) = parse_management(&[OsString::from("aam"), OsString::from("explain")]) else { panic!("parsed") };
        assert_eq!(error.code, "INVALID_ARGUMENT");
        assert!(error.message.contains("필수") || error.message.contains("인수"), "{}", error.message);
        assert!(error.message.contains("tool"), "{}", error.message);
        assert!(error.message.contains("aam"), "{}", error.message);
        assert!(error.message.contains("--help"), "{}", error.message);
        assert!(!error.message.contains("sk-"), "{}", error.message);
    }

    #[test]
    fn unknown_subcommand_does_not_echo_the_token() {
        let token = "sk-live-do-not-echo";
        let Err(error) = parse_management(&[OsString::from("aam"), OsString::from(token)]) else { panic!("parsed") };
        assert!(!error.message.contains(token), "{}", error.message);
        assert!(error.message.contains("명령") || error.message.contains("옵션") || error.message.contains("인수"), "{}", error.message);
        assert!(error.message.contains("--help"), "{}", error.message);
    }
}
