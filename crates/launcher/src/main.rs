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
    about = "AI 계정과 공식 CLI 실행을 관리해요.",
    after_help = "도구 인수는 -- 뒤에 넣어요. 로그인·모델·프로필을 바꾸는 옵션은 막아요. 원래 로그인과 셸 설정은 건드리지 않아요."
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
    #[command(about = "계정, 한도, 대화 상태를 보여 줘요.")]
    Status {
        #[arg(long)]
        json: bool,
    },
    #[command(about = "설치와 아직 확인되지 않은 기능을 점검해요.")]
    Doctor,
    #[command(about = "계정과 한도를 다시 불러와요.")]
    Refresh,
    #[command(about = "계정을 잡지 않고, 왜 그 계정을 골랐는지 봐요.")]
    Explain(Explain),
    #[command(about = "계정을 확인한 뒤 공식 CLI를 실행해요.")]
    Run(Run),
    #[command(about = "한도를 다 쓴 대화를 다른 계정에서 이어요. 기록만 옮기고 로그인은 그대로예요.")]
    Continue {
        #[arg(long, help = "claude 또는 codex. 비우면 이 폴더에서 최근에 쓴 도구")]
        tool: Option<String>,
        #[arg(long, help = "이어 갈 대화 ID. 비우면 이 폴더의 마지막 대화")]
        session: Option<String>,
        #[arg(long, help = "받을 계정 ID. 비우면 원래 계정을 빼고 자동으로 골라요")]
        account: Option<String>,
        #[arg(last = true, help = "도구에 그대로 넘길 인수(예: -- -p \"질문\". Codex는 -- exec \"질문\")")]
        native_args: Vec<std::ffi::OsString>,
    },
    #[command(about = "프로필을 연결하거나 공식 로그인을 시작해요.")]
    Account {
        #[command(subcommand)]
        action: AccountAction,
    },
    #[command(about = "로그인할 때 자동 실행을 켜거나 꺼요.")]
    Service {
        #[command(subcommand)]
        action: ServiceAction,
    },
    #[command(about = "쓸 수 있는 도구만 Ojak 명령으로 연결해요. 원래 CLI는 그대로 둬요.")]
    Integration {
        #[command(subcommand)]
        action: IntegrationAction,
    },
    #[command(about = "omp 로그인을 Ojak에 연결하거나 끊어요.")]
    OmpBroker {
        #[command(subcommand)]
        action: OmpBrokerAction,
    },
    #[command(about = "omp에서 Ojak 계정을 연결하거나 끊어요. 새 세션에서 /model로 ojak-* 모델을 고르세요.")]
    OmpBridge {
        #[command(subcommand)]
        action: OmpBridgeAction,
    },
    #[command(about = "omp에서 계정 사용량만 읽는 확장을 설치하거나 제거해요.")]
    OmpObserver {
        #[command(subcommand)]
        action: ObserverAction,
    },
    #[command(about = "밖에서 시작한 대화를, 확인된 계정으로 넘겨요.")]
    Takeover {
        #[command(subcommand)]
        action: TakeoverAction,
    },
    #[command(about = "처음 쓰는 데 빠진 것만 설치하고, 준비 상태를 JSON으로 보여 줘요.")]
    Setup {
        #[arg(long, conflicts_with = "status", help = "설치하지 않고, 새 터미널에서 명령이 연결됐는지 확인해요.")]
        check: bool,
        #[arg(long, conflicts_with = "with_omp", help = "셸을 실행하지 않고 설정만 확인해요.")]
        status: bool,
        #[arg(long, conflicts_with = "check", help = "같은 방식으로 omp 연결도 준비해요.")]
        with_omp: bool,
    },
    #[command(about = "zsh에 Ojak 명령 경로를 넣거나 빼요.")]
    Shell {
        #[command(subcommand)]
        action: ShellAction,
    },
    #[command(about = "Ojak 연결을 풀어 설치 전으로 돌려요. 계정, 프로필, 로그는 남겨요.")]
    Deactivate {
        #[arg(long, help = "바꾸지 않고 되돌릴 단계만 보여 줘요.")]
        dry_run: bool,
    },
}
#[derive(Args)]
#[command(
    after_help = "실행: aam run claude|codex [--account <계정 ID>] [--model <모델>] [--cwd <프로젝트>] [-- <도구 인수>]\n계정을 비우면 시작할 때 한 번만 골라요. 실행 중에는 바꾸지 않아요. omp는 Ojak이 실행하지 않아요. 새 omp 세션에서 /model로 ojak-* 모델을 고르면 Ojak이 계정을 골라요."
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
    #[command(about = "공식 CLI 프로필을 복사하지 않고 연결해요.")]
    Add {
        #[arg(long)]
        tool: String,
        #[arg(long)]
        label: String,
        #[arg(long)]
        profile: PathBuf,
    },
    #[command(about = "분리된 프로필에서 공식 로그인을 열어요. --account가 있으면 그 프로필에서 다시 로그인해요.")]
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
    #[command(about = "새 프로필에 가져올 수 있는 설정과 빼는 항목을 미리 봐요. 로그인은 복사하지 않아요.")]
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
    #[command(about = "넘겨 둔 대화와, 넘길 수 있는 바깥 대화를 봐요.")]
    List,
    #[command(
        about = "대화의 계정을 확인한 뒤 넘겨요. Claude는 대화 UUID, omp는 대화 파일 경로를 넣어요."
    )]
    Adopt {
        tool: String,
        session: String,
    },
    #[command(about = "넘겨 둔 대화를 해제해요. 이미 만든 대화는 그대로 둬요.")]
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
        .map_err(|_| ApiError::new("INVALID_PATH", "경로에 읽을 수 없는 문자가 있어요."))
}
fn cwd(path: Option<PathBuf>) -> Result<String, ApiError> {
    path.map(path_text).unwrap_or_else(current_cwd)
}
fn print_json(value: &impl serde::Serialize) -> Result<(), ApiError> {
    println!(
        "{}",
        serde_json::to_string_pretty(value).map_err(|_| ApiError::new(
            "SERIALIZATION_FAILED",
            "결과를 만들지 못했어요."
        ))?
    );
    Ok(())
}
fn print_snapshot(snapshot: &Snapshot) {
    println!(
        "Ojak · 계정 {}개 · 자동으로 고르기 {}",
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
            "  로그인: {} · 확인: {}",
            account.auth_status, account.verification
        );
        if let Some(reason) = &account.reason {
            println!("  이유: {reason}");
        }
        for bucket in &account.buckets {
            let usage = bucket
                .used_percent
                .map(|v| format!("{v:.1}%"))
                .unwrap_or_else(|| "알 수 없어요".into());
            println!(
                "  {}: {} · {} · {}",
                bucket.label, usage, bucket.status, bucket.source
            );
        }
    }
    println!("대화 {}개 (끝난 기록 포함)", snapshot.sessions.len());
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
        let observer: Value = serde_json::from_str(&omp_observer::run("status").map_err(|_| ApiError::new("OBSERVER_STATUS_FAILED", "사용량 확인 확장을 확인하지 못했어요."))?)
            .map_err(|_| ApiError::new("PROTOCOL_MISMATCH", "사용량 확인 응답을 읽지 못했어요."))?;
        if observer["current"] != true {
            if observer["installed"] == true {
                omp_observer::run("uninstall").map_err(|_| ApiError::new("OBSERVER_CONFLICT", "확장 소유를 확인하지 못해 바꾸지 않았어요."))?;
            }
            omp_observer::run("install").map_err(|_| ApiError::new("OBSERVER_CONFLICT", "확장을 설치하지 못했어요. 기존 파일과 소유를 확인해 주세요."))?;
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
        .or_else(|| observer.is_none().then(|| "사용량 확인 확장을 확인하지 못했어요. (OBSERVER_STATUS_FAILED)".into()));
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
                    println!("  실행 파일: {path}");
                }
                if let Some(reason) = tool.reason {
                    println!("  {reason}");
                }
            }
            println!("Ojak 명령을 거치지 않는 절대 경로, 다른 앱, 다른 기기의 실행은 관리하지 않아요.");
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
                ApiError::new("INVALID_PROFILE", "프로필 폴더를 확인하지 못했어요.")
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
                            format!("새 터미널의 `{tool}` 명령이 Ojak으로 연결됐어요.")
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
                        println!("omp 로그인 연결: {} · 계정 {}개 · 자동 복구 {} · 설정: {}", if value.connected { "쓸 수 있어요" } else { "연결 안 됨" }, value.account_count.unwrap_or(0), if value.supervised { "켜짐" } else { "꺼짐" }, value.config_path);
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
                        "omp 계정 연결: {} · 계정 통로 {gateways}개 · 고정 대화 {sessions}개 · omp 확장: {}",
                        if value.connected { "연결됨. 새 omp 세션에서 /model로 ojak-* 모델을 고르세요" } else { "연결 안 됨" },
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
                return omp_observer_writer::serve(paths).map_err(|_| ApiError::new("OBSERVER_WRITE_FAILED", "사용량 기록을 안전하게 쓰지 못했어요."));
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
        ErrorKind::MissingRequiredArgument => "필수 항목이 빠졌어요.",
        ErrorKind::MissingSubcommand => "하위 명령이 빠졌어요.",
        ErrorKind::InvalidSubcommand => "알 수 없는 명령이에요.",
        ErrorKind::UnknownArgument => "알 수 없는 옵션이나 인수예요.",
        ErrorKind::InvalidValue | ErrorKind::ValueValidation => "인수 값이 맞지 않아요.",
        ErrorKind::TooManyValues => "인수 값이 너무 많아요.",
        ErrorKind::TooFewValues => "인수 값이 부족해요.",
        ErrorKind::WrongNumberOfValues => "인수 개수가 맞지 않아요.",
        ErrorKind::ArgumentConflict => "함께 쓸 수 없는 옵션이에요.",
        ErrorKind::NoEquals => "이 옵션은 등호(=)로 값을 적어야 해요.",
        ErrorKind::InvalidUtf8 => "인수에 읽을 수 없는 문자가 있어요.",
        _ => "명령 형식이 맞지 않아요.",
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
    message.push_str(" 자세한 형식은 aam --help 또는 해당 명령의 --help를 봐 주세요. 입력한 값은 보여 주지 않았어요.");
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
            ApiError::new("PATHS_UNAVAILABLE", "앱 폴더를 확인하지 못했어요.")
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
                .ok_or_else(|| ApiError::new("INVALID_ARGUMENT", "알 수 없는 명령이에요."))?;
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
            eprintln!("Enter를 누르면 창을 닫아요.");
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
