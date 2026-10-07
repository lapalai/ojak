// Windows 로그인 시 자동 실행될 때 콘솔 창을 띄우지 않는다.
#![cfg_attr(windows, windows_subsystem = "windows")]
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        println!("aam-service [--foreground]\n이 사용자만 쓰는 AI 계정 관리 서비스예요. 로그인하거나 서비스를 설치하지는 않아요.");
        return;
    }
    if args.iter().any(|arg| arg == "--version" || arg == "-V") {
        println!("aam-service {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    if args.iter().any(|arg| arg != "--foreground") {
        eprintln!("지원하지 않는 서비스 옵션이에요. --help를 확인해 주세요.");
        std::process::exit(2);
    }
    let paths = match aam_protocol::Paths::discover() {
        Ok(paths) => paths,
        Err(_) => {
            eprintln!("서비스 상태 폴더를 확인하지 못했어요.");
            std::process::exit(1);
        }
    };
    if let Err(error) = aam_service::run(paths) {
        eprintln!("{}: {}", error.code, error.message);
        std::process::exit(1);
    }
}
