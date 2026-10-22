// Windows 로그인 시 자동 실행될 때 콘솔 창을 띄우지 않는다.
#![cfg_attr(windows, windows_subsystem = "windows")]
use aam_protocol::{ApiError, Paths};

/// 이 프로세스가 맡는 역할.
/// - `Worker`: 서비스를 직접 돈다. macOS(launchd가 감독)의 기본이다.
/// - `Foreground`: 감독 없이 서비스를 직접 돈다(개발·진단용). 모든 OS에서 같다.
/// - `Supervise`: Windows 기본. 같은 실행 파일을 `--worker`로 띄워 감독한다.
/// - `SupervisedWorker`: 감독 프로세스가 띄운 서비스(Windows).
#[derive(Debug, PartialEq, Eq)]
enum Mode {
    Worker,
    Foreground,
    Supervise,
    SupervisedWorker,
}

/// 인자 하나(또는 없음)만 받는다. Windows가 아니면 `--supervise`·`--worker`는 없는 옵션이다.
fn mode(args: &[String]) -> Option<Mode> {
    match args {
        [] if cfg!(windows) => Some(Mode::Supervise),
        [] => Some(Mode::Worker),
        [arg] if arg == "--foreground" => Some(Mode::Foreground),
        [arg] if cfg!(windows) && arg == "--supervise" => Some(Mode::Supervise),
        [arg] if cfg!(windows) && arg == "--worker" => Some(Mode::SupervisedWorker),
        _ => None,
    }
}

/// 이미 다른 인스턴스가 있어 시작하지 않은 경우는 감독 프로세스가 충돌과 구분하도록 전용 종료 코드를 쓴다.
fn exit_status(mode: &Mode, error: &ApiError) -> i32 {
    #[cfg(windows)]
    if matches!(mode, Mode::Supervise | Mode::SupervisedWorker)
        && matches!(
            error.code.as_str(),
            "SERVICE_ALREADY_RUNNING" | "SUPERVISOR_ALREADY_RUNNING"
        )
    {
        return aam_service::supervisor::PEER_EXISTS_EXIT as i32;
    }
    #[cfg(not(windows))]
    let _ = (mode, error);
    1
}

#[cfg(windows)]
fn supervise(paths: Paths) -> Result<(), ApiError> {
    aam_service::supervisor::run(paths)
}

#[cfg(not(windows))]
fn supervise(_: Paths) -> Result<(), ApiError> {
    Err(ApiError::new(
        "PLATFORM_UNSUPPORTED",
        "서비스 감독 프로세스는 Windows에서만 쓸 수 있어요.",
    ))
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        if cfg!(windows) {
            println!("aam-service [--foreground | --supervise]\n이 사용자만 쓰는 AI 계정 관리 서비스예요. 로그인하거나 서비스를 설치하지는 않아요.\n옵션이 없으면 감독 프로세스로 실행해 서비스가 충돌해도 다시 띄워요. --foreground는 감독 없이 이 창에서 직접 실행해요.");
        } else {
            println!("aam-service [--foreground]\n이 사용자만 쓰는 AI 계정 관리 서비스예요. 로그인하거나 서비스를 설치하지는 않아요.");
        }
        return;
    }
    if args.iter().any(|arg| arg == "--version" || arg == "-V") {
        println!("aam-service {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    let Some(mode) = mode(&args) else {
        eprintln!("지원하지 않는 서비스 옵션이에요. --help를 확인해 주세요.");
        std::process::exit(2);
    };
    let paths = match Paths::discover() {
        Ok(paths) => paths,
        Err(_) => {
            eprintln!("서비스 상태 폴더를 확인하지 못했어요.");
            std::process::exit(1);
        }
    };
    // 승격(SSH Session 0 등)으로 시작해도 같은 사용자의 일반 데스크톱이 서비스를 확인할 수 있어야 한다. 실패하면 시작하지 않는다.
    #[cfg(windows)]
    if aam_protocol::winutil::allow_same_user_peer_inspection().is_err() {
        eprintln!("PEER_ACCESS_SETUP_FAILED: 같은 사용자의 다른 로그온 세션이 서비스를 확인할 수 있게 설정하지 못해 시작하지 않았어요.");
        std::process::exit(1);
    }
    let result = match mode {
        Mode::Worker | Mode::Foreground | Mode::SupervisedWorker => aam_service::run(paths),
        Mode::Supervise => supervise(paths),
    };
    if let Err(error) = result {
        eprintln!("{}: {}", error.code, error.message);
        std::process::exit(exit_status(&mode, &error));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn foreground_always_runs_the_service_directly() {
        assert_eq!(mode(&args(&["--foreground"])), Some(Mode::Foreground));
    }

    #[test]
    fn the_default_mode_is_supervision_only_on_windows() {
        let expected = if cfg!(windows) { Mode::Supervise } else { Mode::Worker };
        assert_eq!(mode(&[]), Some(expected));
    }

    #[test]
    fn supervisor_and_worker_flags_exist_only_on_windows() {
        let supervise = mode(&args(&["--supervise"]));
        let worker = mode(&args(&["--worker"]));
        if cfg!(windows) {
            assert_eq!(supervise, Some(Mode::Supervise));
            assert_eq!(worker, Some(Mode::SupervisedWorker));
        } else {
            assert_eq!((supervise, worker), (None, None));
        }
    }

    #[test]
    fn unknown_or_combined_options_are_refused() {
        assert_eq!(mode(&args(&["--bogus"])), None);
        assert_eq!(mode(&args(&["--foreground", "--worker"])), None);
        assert_eq!(mode(&args(&["--foreground", "--foreground"])), None);
        assert_eq!(mode(&args(&["--supervise", "--worker"])), None);
    }

    #[test]
    fn only_supervised_modes_report_an_existing_peer_with_the_dedicated_exit_code() {
        let existing = ApiError::new("SERVICE_ALREADY_RUNNING", "x");
        let supervisor_existing = ApiError::new("SUPERVISOR_ALREADY_RUNNING", "x");
        let other = ApiError::new("SERVICE_IO_ERROR", "x");
        assert_eq!(exit_status(&Mode::Foreground, &existing), 1);
        assert_eq!(exit_status(&Mode::Worker, &existing), 1);
        assert_eq!(exit_status(&Mode::SupervisedWorker, &other), 1);
        if cfg!(windows) {
            assert_eq!(exit_status(&Mode::SupervisedWorker, &existing), 73);
            assert_eq!(exit_status(&Mode::Supervise, &supervisor_existing), 73);
        } else {
            assert_eq!(exit_status(&Mode::SupervisedWorker, &existing), 1);
            assert_eq!(exit_status(&Mode::Supervise, &supervisor_existing), 1);
        }
    }
}
