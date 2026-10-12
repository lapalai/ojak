//! 실행 중 한도 소진이 확인됐을 때 같은 대화를 다른 계정으로 옮기는 흐름의 판단 규칙.
//!
//! 이 모듈은 I/O를 하지 않는다. 언제 자동 이어가기를 켜는지(`Chain::start`), 확인된 증거와 종료·해제 확인이
//! 모두 맞아서 이어도 되는지(`verdict`), 어느 계정으로 옮길지(`choose_target`)만 정한다. 실제 종료·해제·복사·재실행은
//! `lib.rs`와 `supervisor.rs`가 한다.
use crate::arguments;
use aam_protocol::{Account, ApiError, Decision, OmpCredentialPin};
use std::ffi::OsString;

/// 한 번의 사용자 실행에서 자동으로 옮길 수 있는 최대 횟수. 계정 수가 더 적으면 `Chain::visited`가 먼저 막는다.
pub(crate) const MAX_HOPS: usize = 8;

/// 이 대화가 이미 거친 실제 계정(같은 로그인 포함). 한도가 늦게 반영돼도 같은 계정으로 되돌아가지 않게 한다.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Seen {
    id: String,
    provider: String,
    identity_key: Option<String>,
    pins: Vec<OmpCredentialPin>,
}

impl Seen {
    fn of(account: &Account) -> Self {
        Self {
            id: account.id.clone(),
            provider: account.provider.clone(),
            identity_key: account.identity_key.clone().filter(|key| !key.is_empty()),
            pins: account.omp_credential_pins.clone(),
        }
    }

    /// 서비스의 같은 로그인 규칙보다 조건이 느슨하다(확인 단계는 보지 않는다). 의심스러우면 이미 쓴 계정으로 본다.
    fn is(&self, account: &Account) -> bool {
        self.id == account.id
            || (self.provider == account.provider
                && (self.identity_key.is_some() && self.identity_key.as_deref() == account.identity_key.as_deref().filter(|key| !key.is_empty())
                    || self
                        .pins
                        .iter()
                        .any(|pin| !pin.hash.is_empty() && account.omp_credential_pins.contains(pin))))
    }
}

/// 자동 이어가기가 켜진 한 사용자 실행의 상태.
#[derive(Clone, Debug)]
pub(crate) struct Chain {
    visited: Vec<Seen>,
    hops: usize,
    /// 새 계정에서 같은 대화를 다시 열 때 그대로 넘길 안전한 native 옵션. 첫 프롬프트와 대화를 만드는 옵션은 없다.
    carry: Vec<OsString>,
}

/// 켜짐 판단에 쓰는 실행 환경. 터미널 여부와 부모 세션 안인지는 호출자가 읽어 넘긴다(테스트에서 고정할 수 있게).
#[derive(Clone, Copy, Debug)]
pub(crate) struct Environment {
    /// stdin·stdout·stderr가 모두 사용자의 터미널이다.
    pub terminal: bool,
    /// 다른 관리 세션이 띄운 자식이다(부모 세션은 서비스가 다른 계정으로 옮기지 않는다).
    pub nested: bool,
}

impl Chain {
    /// 자동 이어가기를 켤 수 있는 실행이면 상태를 만든다. 확실하지 않으면 `None`이고 기존 동작 그대로다.
    /// - Claude·Codex만, 사용자가 보는 대화형 터미널에서만(출력을 읽는 `--print`·`exec`·`review`와 파이프는 제외).
    /// - 대화 기록을 남기지 않는 실행(`--no-session-persistence`·`--ephemeral`)은 이어 갈 대화가 없다.
    /// - 인수에 새 계정에서 안전하게 되풀이할 수 없는 옵션(`--worktree`·모르는 옵션)이 있으면 켜지 않는다.
    pub(crate) fn start(tool: &str, native_args: &[OsString], env: Environment) -> Option<Self> {
        if !matches!(tool, "claude" | "codex")
            || !env.terminal
            || env.nested
            || arguments::is_ephemeral(native_args)
            || !arguments::is_interactive(tool, native_args)
        {
            return None;
        }
        Some(Self {
            visited: Vec::new(),
            hops: 0,
            carry: arguments::resume_carry(tool, native_args)?,
        })
    }

    pub(crate) fn record(&mut self, account: &Account) {
        if !self.visited.iter().any(|seen| seen.is(account)) {
            self.visited.push(Seen::of(account));
        }
    }

    pub(crate) fn visited(&self, account: &Account) -> bool {
        self.visited.iter().any(|seen| seen.is(account))
    }

    pub(crate) fn hops(&self) -> usize {
        self.hops
    }

    pub(crate) fn carry(&self) -> Vec<OsString> {
        self.carry.clone()
    }

    /// 다음 계정으로 한 번 옮기기로 했다.
    pub(crate) fn advanced(mut self) -> Self {
        self.hops += 1;
        self
    }
}

/// 종료 뒤 확인한 사실. 모든 값은 실행기가 직접 본 것이다(사용량 스냅샷으로 추정하지 않는다).
#[derive(Debug, Default)]
pub(crate) struct Facts<'a> {
    /// 감시자가 확인한 한도 소진 기록의 대화 ID.
    pub exhausted: Option<&'a str>,
    /// 실행기가 한도 소진을 확인해 이 실행의 프로세스를 직접 끝냈다.
    pub stopped: bool,
    /// 사용자의 종료·중단 신호를 받았다.
    pub interrupted: bool,
    /// native 루트와 이 실행의 모든 자손이 끝났음을 실행기가 확인했다.
    pub tree_confirmed: bool,
    /// 서비스가 종료 보고를 받아들여 돌려준 세션 상태.
    pub released_state: Option<&'a str>,
    /// 서비스가 돌려준 세션에 기록된 native 대화 ID.
    pub recorded_native: Option<&'a str>,
    /// 실행 전에 이미 알고 있던 대화 ID(Claude 새 대화·재개).
    pub known_native: Option<&'a str>,
    pub hops: usize,
}

#[derive(Debug)]
pub(crate) enum Verdict {
    /// 한도 소진이 확인됐고 종료·해제·대화 연결이 모두 확인됐다.
    Continue,
    /// 이어갈 이유가 없다(소진 기록이 없거나, 사용자가 끝냈거나, native가 스스로 끝났다). 조용히 기존대로 끝낸다.
    Declined,
    /// 이어가야 하는 상황이지만 안전하게 확인되지 않았다. 옛 예약은 건드리지 않고 사용자에게 알린다.
    Stop(ApiError),
}

pub(crate) fn verdict(facts: &Facts<'_>) -> Verdict {
    let Some(exhausted) = facts.exhausted else {
        return Verdict::Declined;
    };
    // 한도 기록이 있어도 사용자가 직접 끝냈거나 native가 스스로 끝났다면 되살리지 않는다.
    if !facts.stopped || facts.interrupted {
        return Verdict::Declined;
    }
    if !facts.tree_confirmed {
        return Verdict::Stop(ApiError::new(
            "HANDOFF_EXIT_UNCONFIRMED",
            "한도 소진을 확인해 멈췄지만 native 프로세스와 자손이 모두 끝났는지 확인하지 못했어요. 계정 예약은 풀지 않았고 다른 계정으로 옮기지 않았어요. 작업이 끝났는지 확인한 뒤 `aam continue`로 이어 주세요.",
        ));
    }
    if facts.released_state != Some("EXITED") {
        return Verdict::Stop(ApiError::new(
            "HANDOFF_RELEASE_UNCONFIRMED",
            "서비스가 종료를 확인했다고 알려 주지 않아 다른 계정으로 옮기지 않았어요. 서비스가 정리를 마친 뒤 `aam continue`로 이어 주세요.",
        ));
    }
    if facts.known_native.is_some_and(|known| known != exhausted) || facts.recorded_native != Some(exhausted) {
        return Verdict::Stop(ApiError::new(
            "HANDOFF_SESSION_UNVERIFIED",
            "한도 소진이 확인된 대화와 Ojak이 기록한 대화가 일치하지 않아 옮기지 않았어요. 대화 기록은 그대로 있어요.",
        ));
    }
    if facts.hops >= MAX_HOPS {
        return Verdict::Stop(ApiError::new(
            "HANDOFF_LIMIT",
            "한 번의 실행에서 자동으로 옮길 수 있는 횟수에 닿았어요. 한도가 풀린 뒤 `aam continue`로 이어 주세요.",
        ));
    }
    Verdict::Continue
}

/// 이미 거친 계정을 뺀 뒤 새 대화를 받을 수 있는 계정을 고른다. 서비스가 고른 계정이 가능하면 그 계정을 따른다.
/// 서비스는 직전 계정만 빼므로(관측이 늦으면 앞서 한도를 다 쓴 계정으로 되돌아갈 수 있다) 이 목록으로 돌아감을 막는다.
pub(crate) fn choose_target(decision: &Decision, accounts: &[Account], chain: &Chain) -> Option<String> {
    let usable = |id: &str| accounts.iter().find(|account| account.id == id).is_some_and(|account| account.enabled && !chain.visited(account));
    let eligible = |id: &str| decision.candidates.iter().any(|candidate| candidate.account_id == id && candidate.eligible);
    if let Some(selected) = decision.selected_account_id.as_deref() {
        if eligible(selected) && usable(selected) {
            return Some(selected.to_owned());
        }
    }
    decision
        .candidates
        .iter()
        .filter(|candidate| candidate.eligible && usable(&candidate.account_id))
        .max_by(|a, b| {
            a.score
                .unwrap_or(0.0)
                .partial_cmp(&b.score.unwrap_or(0.0))
                .unwrap_or(std::cmp::Ordering::Equal)
                // 점수가 같으면 ID가 앞선 계정(서비스의 정렬 순서와 같다).
                .then_with(|| b.account_id.cmp(&a.account_id))
        })
        .map(|candidate| candidate.account_id.clone())
}

pub(crate) fn no_account_error() -> ApiError {
    ApiError::new(
        "HANDOFF_NO_ACCOUNT",
        "지금 이 대화를 이어 갈 수 있는 다른 계정이 없어요. 기존 대화 기록은 그대로 있어요. 한도가 풀리거나 계정을 추가한 뒤 `aam continue`로 이어 주세요.",
    )
}

/// 옮기는 도중 실패했을 때 사용자가 할 일을 덧붙인다. 이미 안내가 있으면 그대로 둔다.
pub(crate) fn failed_midway(mut error: ApiError) -> ApiError {
    if !error.message.contains("aam continue") {
        error.message = format!(
            "{} 쓰던 프로세스는 이미 멈췄어요. 대화 기록은 그대로 있으니 `aam continue`로 이어 주세요.",
            error.message
        );
    }
    error
}

#[cfg(test)]
mod tests {
    use super::*;
    use aam_protocol::Candidate;

    fn args(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    fn account(id: &str, key: Option<&str>) -> Account {
        Account {
            id: id.into(),
            provider: "anthropic".into(),
            tool: "claude".into(),
            enabled: true,
            identity_key: key.map(Into::into),
            ..Account::default()
        }
    }

    fn candidate(id: &str, eligible: bool, score: Option<f64>) -> Candidate {
        Candidate { account_id: id.into(), eligible, score, reasons: Vec::new() }
    }

    fn decision(selected: Option<&str>, candidates: Vec<Candidate>) -> Decision {
        Decision { selected_account_id: selected.map(Into::into), pin_unavailable: None, candidates, policy_revision: 1 }
    }

    const TTY: Environment = Environment { terminal: true, nested: false };

    fn stopped_facts<'a>(id: &'a str) -> Facts<'a> {
        Facts {
            exhausted: Some(id),
            stopped: true,
            interrupted: false,
            tree_confirmed: true,
            released_state: Some("EXITED"),
            recorded_native: Some(id),
            known_native: Some(id),
            hops: 0,
        }
    }

    #[test]
    fn only_a_watched_interactive_terminal_run_is_enabled() {
        assert!(Chain::start("claude", &args(&["--dangerously-skip-permissions"]), TTY).is_some());
        assert!(Chain::start("claude", &[], TTY).is_some());
        assert!(Chain::start("codex", &args(&["--sandbox", "read-only", "첫 질문"]), TTY).is_some());
        // 출력을 읽는 한 번짜리 실행은 중간에 끝내고 다시 열면 안 된다.
        assert!(Chain::start("claude", &args(&["--print", "질문"]), TTY).is_none());
        assert!(Chain::start("claude", &args(&["-p", "질문"]), TTY).is_none());
        assert!(Chain::start("claude", &args(&["--output-format=json"]), TTY).is_none());
        assert!(Chain::start("codex", &args(&["exec", "질문"]), TTY).is_none());
        assert!(Chain::start("codex", &args(&["--json", "exec", "질문"]), TTY).is_none());
        // 파이프·리다이렉트, 부모 세션 안의 실행, 도구 이름이 다른 경우.
        assert!(Chain::start("claude", &[], Environment { terminal: false, nested: false }).is_none());
        assert!(Chain::start("claude", &[], Environment { terminal: true, nested: true }).is_none());
        assert!(Chain::start("gemini", &[], TTY).is_none());
        // 이어 갈 대화가 없거나 폴더가 바뀌거나 모르는 옵션이 있으면 켜지 않는다.
        assert!(Chain::start("claude", &args(&["--no-session-persistence"]), TTY).is_none());
        assert!(Chain::start("codex", &args(&["--ephemeral"]), TTY).is_none());
        assert!(Chain::start("claude", &args(&["--worktree"]), TTY).is_none());
        assert!(Chain::start("claude", &args(&["--unknown-flag"]), TTY).is_none());
    }

    #[test]
    fn carried_options_keep_safe_settings_and_drop_prompt_and_session_creators() {
        let chain = Chain::start(
            "claude",
            &args(&["--add-dir", "/work", "--fork-session", "--session-id", "x", "--permission-mode", "plan", "--", "첫 프롬프트"]),
            TTY,
        )
        .unwrap();
        assert_eq!(chain.carry(), args(&["--add-dir", "/work", "--permission-mode", "plan"]));
        let chain = Chain::start("codex", &args(&["-s", "workspace-write", "--image", "a.png", "프롬프트"]), TTY).unwrap();
        assert_eq!(chain.carry(), args(&["-s", "workspace-write"]));
    }

    #[test]
    fn visited_accounts_include_the_same_login_under_another_profile() {
        let mut chain = Chain::start("claude", &[], TTY).unwrap();
        chain.record(&account("a", Some("login-1")));
        assert!(chain.visited(&account("a", None)));
        assert!(chain.visited(&account("copy-of-a", Some("login-1"))));
        assert!(!chain.visited(&account("b", Some("login-2"))));
        // identity가 없으면 ID만 같을 때만 같은 계정이다.
        assert!(!chain.visited(&account("c", None)));
        let mut other_provider = account("d", Some("login-1"));
        other_provider.provider = "openai".into();
        assert!(!chain.visited(&other_provider));
    }

    #[test]
    fn target_never_goes_back_to_a_visited_account_even_when_the_service_picks_it() {
        let accounts = [account("a", Some("1")), account("b", Some("2")), account("c", Some("3"))];
        let mut chain = Chain::start("claude", &[], TTY).unwrap();
        chain.record(&accounts[0]);
        chain.record(&accounts[1]);
        // 서비스는 직전 계정(b)만 빼므로 한도 관측이 늦으면 이미 한도를 다 쓴 a를 고를 수 있다.
        let picked_visited = decision(Some("a"), vec![candidate("a", true, Some(9.0)), candidate("b", false, None), candidate("c", true, Some(1.0))]);
        assert_eq!(choose_target(&picked_visited, &accounts, &chain).as_deref(), Some("c"));
        // 쓸 수 있는 새 계정이 없으면 고르지 않는다(되돌아가지 않는다).
        let only_visited = decision(Some("a"), vec![candidate("a", true, Some(9.0)), candidate("b", false, None), candidate("c", false, None)]);
        assert_eq!(choose_target(&only_visited, &accounts, &chain), None);
        let none = decision(None, vec![candidate("a", false, None)]);
        assert_eq!(choose_target(&none, &accounts, &chain), None);
    }

    #[test]
    fn target_follows_the_service_choice_and_otherwise_the_best_eligible_score() {
        let accounts = [account("a", Some("1")), account("b", Some("2")), account("c", Some("3"))];
        let mut chain = Chain::start("claude", &[], TTY).unwrap();
        chain.record(&accounts[0]);
        let decided = decision(Some("c"), vec![candidate("b", true, Some(9.0)), candidate("c", true, Some(1.0))]);
        assert_eq!(choose_target(&decided, &accounts, &chain).as_deref(), Some("c"));
        // 서비스의 선택이 자격 없음으로 표시돼 있으면 따르지 않는다.
        let stale_pick = decision(Some("c"), vec![candidate("b", true, Some(9.0)), candidate("c", false, None)]);
        assert_eq!(choose_target(&stale_pick, &accounts, &chain).as_deref(), Some("b"));
        // 비활성 계정은 후보여도 고르지 않는다.
        let mut disabled = accounts.clone();
        disabled[1].enabled = false;
        let only_disabled = decision(None, vec![candidate("b", true, Some(9.0))]);
        assert_eq!(choose_target(&only_disabled, &disabled, &chain), None);
        // 점수가 같으면 ID가 앞선 계정.
        let tied = decision(None, vec![candidate("c", true, Some(2.0)), candidate("b", true, Some(2.0))]);
        assert_eq!(choose_target(&tied, &accounts, &chain).as_deref(), Some("b"));
    }

    #[test]
    fn verdict_continues_only_after_stop_exit_and_release_are_all_confirmed() {
        let id = "11111111-2222-4333-8444-555555555555";
        assert!(matches!(verdict(&stopped_facts(id)), Verdict::Continue));
        let code = |facts: Facts<'_>| match verdict(&facts) {
            Verdict::Stop(error) => error.code,
            Verdict::Declined => "declined".into(),
            Verdict::Continue => "continue".into(),
        };
        // 한도 기록이 없으면 사용량 스냅샷만으로 되살리지 않는다.
        assert_eq!(code(Facts { exhausted: None, ..stopped_facts(id) }), "declined");
        // native가 스스로 끝났거나 사용자가 끝냈다.
        assert_eq!(code(Facts { stopped: false, ..stopped_facts(id) }), "declined");
        assert_eq!(code(Facts { interrupted: true, ..stopped_facts(id) }), "declined");
        // 트리 종료·해제 확인이 없으면 옮기지 않고 멈춘다.
        assert_eq!(code(Facts { tree_confirmed: false, ..stopped_facts(id) }), "HANDOFF_EXIT_UNCONFIRMED");
        for state in [None, Some("ORPHANED"), Some("SUSPECT"), Some("ACTIVE")] {
            assert_eq!(code(Facts { released_state: state, ..stopped_facts(id) }), "HANDOFF_RELEASE_UNCONFIRMED", "{state:?}");
        }
        // 대화 연결이 어긋나면 멈춘다.
        assert_eq!(code(Facts { recorded_native: None, ..stopped_facts(id) }), "HANDOFF_SESSION_UNVERIFIED");
        assert_eq!(code(Facts { known_native: Some("99999999-2222-4333-8444-555555555555"), ..stopped_facts(id) }), "HANDOFF_SESSION_UNVERIFIED");
        // 새 Codex 대화는 시작 때 ID를 모른다. 서비스가 같은 ID를 기록했으면 계속한다.
        assert!(matches!(verdict(&Facts { known_native: None, ..stopped_facts(id) }), Verdict::Continue));
        // 횟수 한도.
        assert_eq!(code(Facts { hops: MAX_HOPS, ..stopped_facts(id) }), "HANDOFF_LIMIT");
    }

    #[test]
    fn midway_failures_tell_the_user_how_to_resume_once() {
        let first = failed_midway(ApiError::new("CONTINUE_TRANSCRIPT_MISSING", "대화 기록을 찾지 못했어요."));
        assert!(first.message.contains("`aam continue`"));
        assert_eq!(first.code, "CONTINUE_TRANSCRIPT_MISSING");
        let again = failed_midway(first.clone());
        assert_eq!(again.message, first.message);
    }

    #[test]
    fn hops_advance_one_at_a_time() {
        let chain = Chain::start("claude", &[], TTY).unwrap();
        assert_eq!(chain.hops(), 0);
        assert_eq!(chain.advanced().hops(), 1);
    }
}
