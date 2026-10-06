// Windows 로그인 시 자동 실행될 때 콘솔 창을 띄우지 않는다.
#![cfg_attr(windows, windows_subsystem = "windows")]
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        println!("aam-service [--foreground]\n사용자 전용 AI 계정 관리 서비스입니다. 로그인하거나 서비스를 설치하지 않습니다.");
        return;
    }
    if args.iter().any(|arg| arg == "--version" || arg == "-V") {
        println!("aam-service {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    if args.iter().any(|arg| arg != "--foreground") {
        eprintln!("지원하지 않는 서비스 옵션입니다. --help를 확인하세요.");
        std::process::exit(2);
    }
    let paths = match aam_protocol::Paths::discover() {
        Ok(paths) => paths,
        Err(_) => {
            eprintln!("서비스 상태 경로를 확인할 수 없습니다.");
            std::process::exit(1);
        }
    };
    if let Err(error) = aam_service::run(paths) {
        eprintln!("{}: {}", error.code, error.message);
        std::process::exit(1);
    }
}
