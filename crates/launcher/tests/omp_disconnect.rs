use aam_protocol::secure::{restrict_dir, restrict_file};
use sha2::{Digest, Sha256};
use std::{fs, path::PathBuf, process::{Command, Output}};

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("ojak-disconnect-{}", aam_protocol::new_id()));
        restrict_dir(&root).unwrap();
        Self(root.canonicalize().unwrap())
    }
    fn disconnect(&self, omp: &std::path::Path) -> Output {
        Command::new(env!("CARGO_BIN_EXE_aam"))
            .args(["omp-bridge", "disconnect"])
            .env("HOME", &self.0)
            .env("USERPROFILE", &self.0)
            .env("AAM_HOME", self.0.join("aam"))
            .env("PI_CODING_AGENT_DIR", self.0.join("agent"))
            .env("OMP_NATIVE_BIN", omp)
            .env_remove("AAM_HOLD_ON_ERROR")
            .output().unwrap()
    }
    fn owned_extension(&self) -> PathBuf {
        let directory = self.0.join("agent/extensions/aam-accounts");
        restrict_dir(&directory).unwrap();
        let source = b"export const previousVersion = true;\n";
        let receipt = serde_json::json!({
            "owner": "ai-account-manager.omp-accounts.v1", "version": 1,
            "sha256": format!("{:x}", Sha256::digest(source)),
        });
        for (name, bytes) in [("index.js", source.to_vec()), (".aam-owner.json", receipt.to_string().into_bytes())] {
            let path = directory.join(name);
            fs::write(&path, bytes).unwrap();
            restrict_file(&path).unwrap();
        }
        directory
    }
}
impl Drop for Fixture {
    fn drop(&mut self) { let _ = fs::remove_dir_all(&self.0); }
}

#[test]
fn never_connected_omp_does_not_require_an_omp_installation_or_create_state() {
    let fixture = Fixture::new();
    for _ in 0..2 {
        let output = fixture.disconnect(&fixture.0.join("missing-omp"));
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        let status: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(status["connected"], false);
        assert_eq!(status["extensionInstalled"], false);
    }
    assert!(!fixture.0.join("aam").exists());
    assert!(!fixture.0.join("agent").exists());
}

#[test]
fn failed_config_read_preserves_owned_extension_for_retry() {
    let fixture = Fixture::new();
    let directory = fixture.owned_extension();
    let entry = fs::read(directory.join("index.js")).unwrap();
    let receipt = fs::read(directory.join(".aam-owner.json")).unwrap();
    // Missing runtime and an executable rejecting `config get` are distinct failure modes.
    for omp in [fixture.0.join("missing-omp"), PathBuf::from(env!("CARGO_BIN_EXE_aam"))] {
        let output = fixture.disconnect(&omp);
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("OMP_CONFIG_FAILED"));
        assert_eq!(fs::read(directory.join("index.js")).unwrap(), entry);
        assert_eq!(fs::read(directory.join(".aam-owner.json")).unwrap(), receipt);
        assert!(!fixture.0.join("aam/bridge.json").exists());
    }
}
