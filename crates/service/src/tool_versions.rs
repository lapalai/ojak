//! 설치된 공식 CLI·omp가 최신보다 오래됐는지 본다.
//!
//! 이유: 공급자 서버는 오래된 클라이언트 버전에 새 모델을 오류 없이 숨긴다(2026-11-19, Codex 0.154.0에서 `gpt-6.2-astra`가
//! 목록에 없었다). 사용자가 먼저 발견하기 전에 Ojak이 알려 준다.
//!
//! 조회는 읽기 전용이다. 버전 번호만 받고 계정·토큰·경로는 보내지 않는다. 실패하면 모른다고 두고 '최신'이라고 말하지 않는다.
//! 업데이트는 하지 않는다. 사용자가 공식 명령으로 직접 한다.

use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, time::Duration};

pub(crate) const METADATA_KEY: &str = "toolLatestVersions";
/// 6시간마다 다시 본다(upstream omp의 Codex 버전 추적 주기와 같다).
pub(crate) const REFRESH_MS: i64 = 6 * 60 * 60 * 1000;
/// 릴리스 목록 응답은 본문(릴리스 노트)까지 실려 수백 KB다. 느린 회선에서도 끝까지 받게 넉넉히 둔다.
const TIMEOUT: Duration = Duration::from_secs(30);
const MAX_BODY: usize = 4 * 1024 * 1024;

const CLAUDE_LATEST: &str = "https://registry.npmjs.org/@anthropic-ai/claude-code/latest";
const CODEX_LATEST: &str = "https://registry.npmjs.org/@openai/codex/latest";
/// omp는 Ojak 호환판만 쓴다. 최신은 upstream이 아니라 Ojak이 게시한 호환판이다.
const OMP_RELEASES: &str = "https://api.github.com/repos/lapalai/ojak/releases?per_page=30";

/// 마지막으로 확인한 도구별 최신 버전. 확인하지 못한 도구는 빠져 있다.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Latest {
    pub checked_at: i64,
    pub versions: BTreeMap<String, String>,
}

/// `2.1.328 (Claude Code)`, `codex-cli 0.170.0`, `omp/18.22.2-ojak.1` 같은 출력에서 버전 숫자와 Ojak 판 번호를 뽑는다.
/// 읽을 수 없으면 None이다(추정하지 않는다).
fn parse(text: &str) -> Option<(Vec<u64>, u64)> {
    let start = text.find(|c: char| c.is_ascii_digit())?;
    let token: &str = text[start..].split(|c: char| c.is_whitespace() || c == ')').next()?;
    let (core, fork) = match token.split_once("-ojak.") {
        Some((core, fork)) => (core, fork.parse().ok()?),
        None => (token.split('-').next()?, 0),
    };
    let parts: Vec<u64> = core.split('.').map(|part| part.parse().ok()).collect::<Option<_>>()?;
    (parts.len() >= 2).then_some((parts, fork))
}

/// 설치된 버전이 최신보다 낮을 때만 true다. 어느 쪽이든 읽을 수 없으면 false(모름)다.
pub(crate) fn outdated(installed: Option<&str>, latest: Option<&str>) -> bool {
    match (installed.and_then(parse), latest.and_then(parse)) {
        (Some(installed), Some(latest)) => installed < latest,
        _ => false,
    }
}

fn client() -> Option<Client> {
    Client::builder().timeout(TIMEOUT).redirect(reqwest::redirect::Policy::none()).user_agent(concat!("ojak/", env!("CARGO_PKG_VERSION"))).build().ok()
}

fn json(client: &Client, url: &str) -> Option<serde_json::Value> {
    let response = client.get(url).header("Accept", "application/json").send().ok()?;
    if !response.status().is_success() { return None; }
    let body = response.bytes().ok()?;
    if body.len() > MAX_BODY { return None; }
    serde_json::from_slice(&body).ok()
}

fn npm_latest(client: &Client, url: &str) -> Option<String> {
    let version = json(client, url)?.get("version")?.as_str()?.to_owned();
    parse(&version).map(|_| version)
}

/// 게시된(초안·시험판 아님) `omp-vX.Y.Z-ojak.N` 릴리스 가운데 가장 높은 버전.
pub(crate) fn newest_omp(releases: &serde_json::Value) -> Option<String> {
    releases.as_array()?.iter()
        .filter(|release| release["draft"] == false && release["prerelease"] == false)
        .filter_map(|release| release["tag_name"].as_str()?.strip_prefix("omp-v"))
        .filter(|version| version.contains("-ojak."))
        .filter_map(|version| parse(version).map(|key| (key, version.to_owned())))
        .max_by(|a, b| a.0.cmp(&b.0))
        .map(|(_, version)| version)
}

/// 세 도구의 최신 버전을 읽는다. 읽지 못한 도구는 결과에서 뺀다.
pub(crate) fn fetch(now: i64) -> Latest {
    let mut versions = BTreeMap::new();
    if let Some(client) = client() {
        if let Some(version) = npm_latest(&client, CLAUDE_LATEST) { versions.insert("claude".to_owned(), version); }
        if let Some(version) = npm_latest(&client, CODEX_LATEST) { versions.insert("codex".to_owned(), version); }
        if let Some(version) = json(&client, OMP_RELEASES).as_ref().and_then(newest_omp) { versions.insert("omp".to_owned(), version); }
    }
    Latest { checked_at: now, versions }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn outdated_only_when_both_versions_are_readable_and_installed_is_lower() {
        // 실제 `--version` 출력 모양.
        assert!(outdated(Some("codex-cli 0.154.0"), Some("0.170.0")));
        assert!(!outdated(Some("codex-cli 0.170.0"), Some("0.170.0")));
        assert!(outdated(Some("2.1.317 (Claude Code)"), Some("2.1.328")));
        // 숫자로 비교한다: 0.9.0 < 0.10.0.
        assert!(outdated(Some("0.9.0"), Some("0.10.0")));
        // 설치본이 더 새롭거나(로컬 빌드) 같으면 알리지 않는다.
        assert!(!outdated(Some("2.2.0"), Some("2.1.328")));
        // omp는 같은 upstream 버전이면 Ojak 판 번호로 비교한다.
        assert!(outdated(Some("omp/18.18.9-ojak.1"), Some("18.22.2-ojak.1")));
        assert!(outdated(Some("omp/18.22.2-ojak.1"), Some("18.22.2-ojak.2")));
        assert!(!outdated(Some("omp/18.22.2-ojak.1"), Some("18.22.2-ojak.1")));
        // 모르면 오래됐다고 하지 않는다.
        assert!(!outdated(None, Some("1.0.0")));
        assert!(!outdated(Some("1.0.0"), None));
        assert!(!outdated(Some("unknown"), Some("1.0.0")));
        assert!(!outdated(Some("1.0.0"), Some("latest")));
    }

    #[test]
    fn newest_omp_ignores_drafts_prereleases_app_tags_and_plain_upstream_tags() {
        let releases = json!([
            {"tag_name": "v0.28.1", "draft": false, "prerelease": false},
            {"tag_name": "omp-v19.0.0-ojak.1", "draft": true, "prerelease": false},
            {"tag_name": "omp-v18.30.0-ojak.1", "draft": false, "prerelease": true},
            {"tag_name": "omp-v18.15.0-ojak.1", "draft": false, "prerelease": false},
            {"tag_name": "omp-v18.22.2-ojak.1", "draft": false, "prerelease": false},
            {"tag_name": "omp-v18.9.0-ojak.3", "draft": false, "prerelease": false},
            {"tag_name": "omp-v99.0.0", "draft": false, "prerelease": false},
        ]);
        assert_eq!(newest_omp(&releases).as_deref(), Some("18.22.2-ojak.1"));
        assert_eq!(newest_omp(&json!([])), None);
        assert_eq!(newest_omp(&json!({"message": "rate limited"})), None);
    }

    /// 실제 npm·GitHub에 물어 세 도구의 버전이 읽히는지 본다. 네트워크가 필요해 평소에는 돌리지 않는다.
    #[test]
    #[ignore = "needs network"]
    fn live_fetch_reads_all_three_tools() {
        let latest = fetch(1);
        println!("{:?}", latest.versions);
        for tool in ["claude", "codex", "omp"] {
            assert!(latest.versions.get(tool).is_some_and(|version| parse(version).is_some()), "{tool}");
        }
        assert!(latest.versions["omp"].contains("-ojak."));
    }
}
