use crate::omp_extension::{extension_directory, failure, inspect, install, uninstall, Extension, Result};
use serde_json::json;

const OBSERVER: Extension = Extension {
    source: include_bytes!("../../../integrations/omp/aam-observer.js"),
    owner: "ai-account-manager.omp-observer.v1",
    directory: "aam-observer",
};

pub fn run(action: &str) -> Result<String> {
    let directory = extension_directory(&OBSERVER)?;
    if action == "status" {
        return match inspect(&OBSERVER, &directory) {
            Ok(installed) => Ok(json!({
                "installed": installed.is_some(),
                "current": installed.as_ref().is_some_and(|value| value.current),
                "collision": false,
                "path": directory,
                "supportedOmpVersion": "18.4.4",
                "futureLaunchesOnly": true,
            }).to_string()),
            Err(_) => Ok(json!({ "installed": false, "collision": true, "path": directory, "futureLaunchesOnly": true }).to_string()),
        };
    }
    let changed = match action {
        "install" => install(&OBSERVER, &directory)?,
        "uninstall" => uninstall(&OBSERVER, &directory)?,
        _ => return Err(failure("지원하지 않는 관측 확장 작업입니다.")),
    };
    let installed = inspect(&OBSERVER, &directory)?.is_some();
    Ok(json!({ "installed": installed, "changed": changed, "path": directory, "futureLaunchesOnly": true }).to_string())
}
