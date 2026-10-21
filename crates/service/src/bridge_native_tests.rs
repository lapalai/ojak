//! 인증 갱신 단일 비행·경합 회귀 테스트. 실제 계정·CLI 없이 제어 가능한 loader와 loopback 소켓만 쓴다.
use super::*;
use std::{
    net::{SocketAddr, TcpListener},
    sync::{atomic::AtomicBool, mpsc},
    time::Instant,
};

const IDENTITY: &str = "email:a@example.test|org:org-a";
const KEY_A: &str = "anthropic|email:a@example.test|workspace:org-a";

struct Directory(PathBuf);
impl std::ops::Deref for Directory {
    type Target = Path;
    fn deref(&self) -> &Path { &self.0 }
}
impl Drop for Directory {
    fn drop(&mut self) { let _ = fs::remove_dir_all(&self.0); }
}

struct Env {
    service: Arc<Service>,
    // 서비스·SQLite 핸들이 먼저 닫힌 뒤 지운다(Windows는 열린 DB 파일을 지울 수 없다).
    dir: Directory,
}
impl Env {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("ojak-native-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let paths = Paths { socket: dir.join("service.sock"), database: dir.join("state.db"), profiles: dir.join("profiles"), home: dir.clone() };
        Self { service: Service::open(paths).unwrap(), dir: Directory(dir) }
    }
    /// 프로필 경로는 프로세스 전역 점유 표의 키이므로 테스트마다 유일해야 한다.
    fn account(&self) -> Account {
        let id = uuid::Uuid::new_v4().to_string();
        Account {
            id: format!("acct-{id}"),
            tool: "claude".into(),
            enabled: true,
            auth_status: "authenticated".into(),
            email: Some("a@example.test".into()),
            identity_key: Some(KEY_A.into()),
            profile_path: Some(self.dir.join(format!("profile-{id}")).to_string_lossy().into_owned()),
            binary_path: Some("/nonexistent/claude".into()),
            ..Account::default()
        }
    }
    fn save(&self, account: &Account) {
        crate::store::save_account(&self.service.lock().unwrap().connection, account).unwrap();
    }
}

/// 열기 전까지 막히고, 호출 횟수를 센다. `honor_deadline=false`는 마감을 무시하는 동기 syscall을 흉내 낸다.
struct Fake {
    calls: std::sync::atomic::AtomicUsize,
    gate: Lock<bool>,
    gate_changed: Condvar,
    outcome: Lock<Result<String, String>>,
    honor_deadline: bool,
    panics: AtomicBool,
    /// 가짜 loader가 보고하는 단계. 실제 어댑터가 단계를 알리는 방식과 같은 호출이다.
    stage: Lock<&'static str>,
}
impl Fake {
    fn new(open: bool, honor_deadline: bool) -> Arc<Self> {
        Arc::new(Self {
            calls: Default::default(),
            gate: Lock::new(open),
            gate_changed: Condvar::new(),
            outcome: Lock::new(Ok("token-1".into())),
            honor_deadline,
            panics: AtomicBool::new(false),
            stage: Lock::new("cli-refresh"),
        })
    }
    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
    fn open(&self) {
        *self.gate.lock() = true;
        self.gate_changed.notify_all();
    }
    fn set_ok(&self, token: &str) {
        *self.outcome.lock() = Ok(token.into());
    }
    fn set_err(&self, code: &str) {
        *self.outcome.lock() = Err(code.into());
    }
    fn set_stage(&self, stage: &'static str) {
        *self.stage.lock() = stage;
    }
    fn loader(self: &Arc<Self>) -> Loader {
        let fake = Arc::clone(self);
        Arc::new(move |_: &Account, _: bool, deadline: Instant, stage: &mut dyn FnMut(&'static str)| fake.load(deadline, stage))
    }
    fn load(&self, deadline: Instant, stage: &mut dyn FnMut(&'static str)) -> Result<NativeAccess, ApiError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        stage(*self.stage.lock());
        let mut open = self.gate.lock();
        while !*open {
            if self.honor_deadline {
                if self.gate_changed.wait_until(&mut open, deadline).timed_out() && !*open {
                    return Err(ApiError::new("AUTH_REFRESH_TIMEOUT", "deadline"));
                }
            } else {
                self.gate_changed.wait(&mut open);
            }
        }
        drop(open);
        if self.panics.load(Ordering::SeqCst) {
            panic!("controlled loader panic");
        }
        match &*self.outcome.lock() {
            Ok(token) => Ok(NativeAccess { access: token.clone(), expires: i64::MAX, account_id: None }),
            Err(code) => Err(ApiError::new(code, "controlled failure")),
        }
    }
}

fn source_with(env: &Env, account: Account, fake: &Arc<Fake>, budget: &Arc<RefreshBudget>, job_budget: Duration, cooldown: Duration) -> Arc<Source> {
    env.save(&account);
    Arc::new(
        Source::new(account, "anthropic", IDENTITY.into(), Arc::downgrade(&env.service), Arc::clone(budget))
            .with_loader(fake.loader(), job_budget, cooldown),
    )
}
fn source(env: &Env, fake: &Arc<Fake>) -> Arc<Source> {
    source_with(env, env.account(), fake, &Arc::new(RefreshBudget::new(4)), Duration::from_secs(10), Duration::from_millis(400))
}
fn seed(source: &Source, token: &str, expires: i64) {
    let mut state = source.state.lock();
    state.access = Some(NativeAccess { access: token.into(), expires, account_id: None });
    state.checked_at = now_ms();
    state.generation = 7;
}
fn call(source: &Arc<Source>, force: bool, within: Duration) -> Result<(), ApiError> {
    source.ensure(force, Instant::now() + within, &|| false)
}
fn spawn_call(source: &Arc<Source>, force: bool, within: Duration) -> std::thread::JoinHandle<Result<(), ApiError>> {
    let source = Arc::clone(source);
    std::thread::spawn(move || call(&source, force, within))
}
fn wait_until(what: &str, condition: impl Fn() -> bool) {
    let end = Instant::now() + Duration::from_secs(5);
    while !condition() {
        assert!(Instant::now() < end, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}
fn code(result: Result<(), ApiError>) -> String {
    result.unwrap_err().code
}

/// 소스를 loopback에 노출한다. 연결마다 `handle`이 끝나면 `done`으로 알린다.
fn serve(source: &Arc<Source>, request_deadline: Duration) -> (SocketAddr, mpsc::Receiver<()>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let (done_tx, done_rx) = mpsc::channel();
    let source = Arc::downgrade(source);
    std::thread::spawn(move || {
        while source.strong_count() > 0 {
            let stream = match listener.accept() {
                Ok((stream, _)) => stream,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(5));
                    continue;
                }
                Err(_) => break,
            };
            let Some(source) = source.upgrade() else { break };
            let done = done_tx.clone();
            std::thread::spawn(move || {
                let mut stream = stream;
                let (request, mut rest) = super::super::read_head(&mut stream, Instant::now() + Duration::from_secs(5)).unwrap();
                let length: usize = request.header("content-length").and_then(|v| v.parse().ok()).unwrap_or(0);
                let mut chunk = [0u8; 4096];
                while rest.len() < length {
                    let read = stream.read(&mut chunk).unwrap();
                    rest.extend_from_slice(&chunk[..read]);
                }
                let route = if request.query.is_empty() { request.path.clone() } else { format!("{}?{}", request.path, request.query) };
                let request = Request { body: rest, ..request };
                source.handle(&mut stream, &request, &route, Instant::now() + request_deadline);
                let _ = done.send(());
            });
        }
    });
    (address, done_rx)
}
fn connect(address: SocketAddr, head: &str, body: &str) -> TcpStream {
    let mut stream = TcpStream::connect(address).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    write!(stream, "{head}\r\nHost: x\r\nContent-Length: {}\r\n\r\n{body}", body.len()).unwrap();
    stream
}
fn read_all(mut stream: TcpStream) -> String {
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    response
}
fn exchange(address: SocketAddr, head: &str, body: &str) -> (Duration, String) {
    let started = Instant::now();
    let response = read_all(connect(address, head, body));
    (started.elapsed(), response)
}
fn block_body() -> String {
    json!({"providerKey":"anthropic","blockScope":"model:x","blockedUntilMs":now_ms() + 60_000}).to_string()
}

#[test]
fn native_identity_keeps_workspaces_separate_and_requires_verified_login() {
    let mut account = Account {
        tool: "claude".into(),
        enabled: true,
        auth_status: "authenticated".into(),
        email: Some("A@example.test".into()),
        identity_key: Some("anthropic|email:a@example.test|workspace:org-a".into()),
        ..Account::default()
    };
    assert_eq!(identity(&account), Some(("anthropic", "email:a@example.test|org:org-a".into())));
    account.identity_key = Some("anthropic|email:a@example.test|workspace:org-b".into());
    assert_eq!(identity(&account), Some(("anthropic", "email:a@example.test|org:org-b".into())));
    account.enabled = false;
    assert!(identity(&account).is_none());
    account.enabled = true;
    account.auth_status = "auth-required".into();
    assert!(identity(&account).is_none());
}

#[test]
fn rejected_access_stays_disabled_until_native_token_changes() {
    let env = Env::new();
    let source = source(&env, &Fake::new(true, true));
    let mut state = SourceState {
        access: Some(NativeAccess { access: "old".into(), expires: i64::MAX, account_id: None }),
        ..SourceState::default()
    };
    assert!(source.entry(&state).is_some());
    state.disabled = Some(Sha256::digest(b"old").into());
    assert!(source.entry(&state).is_none());
    state.access.as_mut().unwrap().access = "new".into();
    let entry = source.entry(&state).unwrap();
    assert_eq!(entry["credential"]["refresh"], "__remote__");
    assert_eq!(entry["credential"]["access"], "new");
}

#[test]
fn concurrent_force_refreshes_for_one_profile_run_the_loader_once() {
    let env = Env::new();
    let fake = Fake::new(false, true);
    let source = source(&env, &fake);
    let waiters: Vec<_> = (0..8).map(|_| spawn_call(&source, true, Duration::from_secs(5))).collect();
    wait_until("first loader call", || fake.calls() == 1);
    std::thread::sleep(Duration::from_millis(150));
    assert_eq!(fake.calls(), 1, "waiters started their own CLI refresh");
    fake.open();
    for waiter in waiters {
        waiter.join().unwrap().unwrap();
    }
    assert_eq!(fake.calls(), 1);
    assert_eq!(source.state.lock().access.as_ref().unwrap().access, "token-1");
    assert_eq!(source.budget.active(), 0);
}

#[test]
fn one_failure_is_shared_then_cooled_down_then_recoverable() {
    let env = Env::new();
    let fake = Fake::new(false, true);
    fake.set_err("NATIVE_KEYCHAIN_LOCKED");
    let source = source(&env, &fake);
    let first = spawn_call(&source, true, Duration::from_secs(5));
    wait_until("first loader call", || fake.calls() == 1);
    let rest: Vec<_> = (0..5).map(|_| spawn_call(&source, true, Duration::from_secs(5))).collect();
    std::thread::sleep(Duration::from_millis(100));
    fake.open();
    assert_eq!(code(first.join().unwrap()), "NATIVE_KEYCHAIN_LOCKED");
    for waiter in rest {
        assert_eq!(code(waiter.join().unwrap()), "NATIVE_KEYCHAIN_LOCKED");
    }
    assert_eq!(fake.calls(), 1, "each waiter restarted the CLI");
    // 실패 직후 새 요청도 CLI를 다시 띄우지 않고 같은 실패를 본다.
    assert_eq!(code(call(&source, true, Duration::from_secs(1))), "NATIVE_KEYCHAIN_LOCKED");
    assert_eq!(fake.calls(), 1);
    assert!(source.state.lock().access.is_none());
    // cooldown은 유한하다. 지나면 다음 갱신이 실제로 실행되고 성공한다.
    std::thread::sleep(Duration::from_millis(450));
    fake.set_ok("token-2");
    call(&source, true, Duration::from_secs(2)).unwrap();
    assert_eq!(fake.calls(), 2);
    assert_eq!(source.state.lock().access.as_ref().unwrap().access, "token-2");
}

#[test]
fn another_profile_and_short_operations_are_not_blocked_by_a_slow_refresh() {
    let env = Env::new();
    let slow = Fake::new(false, true);
    let quick = Fake::new(true, true);
    let slow_source = source(&env, &slow);
    let quick_source = source(&env, &quick);
    seed(&slow_source, "t0", i64::MAX);
    let (address, _) = serve(&slow_source, Duration::from_secs(30));
    let pending = connect(address, "POST /v1/credential/1/refresh HTTP/1.1", "");
    wait_until("slow refresh start", || slow.calls() == 1);

    let started = Instant::now();
    call(&quick_source, true, Duration::from_secs(2)).unwrap();
    assert!(started.elapsed() < Duration::from_secs(1), "other profile waited for the slow refresh");

    for (head, body) in [
        ("GET /v1/healthz HTTP/1.1", String::new()),
        ("POST /v1/credential/1/block HTTP/1.1", block_body()),
        ("POST /v1/credential/1/disable HTTP/1.1", String::new()),
        ("DELETE /v1/credential/1/blocks HTTP/1.1", String::new()),
    ] {
        let (elapsed, response) = exchange(address, head, &body);
        assert!(response.starts_with("HTTP/1.1 200 "), "{head}: {response}");
        assert!(elapsed < Duration::from_secs(1), "{head} blocked behind refresh for {elapsed:?}");
    }
    assert_eq!(slow.calls(), 1);

    // 같은 token이 다시 돌아오면 거절 표시가 유지되어 복구된 것으로 게시하지 않는다.
    slow.set_ok("t0");
    slow.open();
    let response = read_all(pending);
    assert!(response.starts_with("HTTP/1.1 401 "), "{response}");
    assert!(!response.contains("\"access\""));
}

#[test]
fn a_rejected_token_recovers_only_when_the_cli_returns_a_different_one() {
    let env = Env::new();
    let fake = Fake::new(true, true);
    let source = source(&env, &fake);
    seed(&source, "bad", i64::MAX);
    let (address, _) = serve(&source, Duration::from_secs(30));
    assert!(exchange(address, "POST /v1/credential/1/disable HTTP/1.1", "").1.starts_with("HTTP/1.1 200 "));
    fake.set_ok("bad");
    assert!(exchange(address, "POST /v1/credential/1/refresh HTTP/1.1", "").1.starts_with("HTTP/1.1 401 "));
    fake.set_ok("good");
    let (_, response) = exchange(address, "POST /v1/credential/1/refresh HTTP/1.1", "");
    assert!(response.starts_with("HTTP/1.1 200 "), "{response}");
    assert!(response.contains("\"access\":\"good\""));
    assert!(source.ready().is_ok());
}

#[test]
fn a_stale_or_failed_credential_is_never_served_in_place_of_the_new_one() {
    let env = Env::new();
    let fake = Fake::new(true, true);
    fake.set_err("AUTH_REQUIRED");
    let source = source(&env, &fake);
    // 만료된 token은 신선하지 않으므로 갱신을 거쳐야 하고, 갱신이 인증 실패면 옛 token을 내보내지 않는다.
    seed(&source, "expired-token", now_ms() - 1);
    let (address, _) = serve(&source, Duration::from_secs(30));
    let (_, response) = exchange(address, "GET /v1/snapshot?wait=0 HTTP/1.1", "");
    assert!(response.starts_with("HTTP/1.1 401 "), "{response}");
    assert!(!response.contains("expired-token"));
    assert!(source.state.lock().access.is_none());
}

#[test]
fn a_failed_forced_refresh_does_not_keep_serving_the_previous_fresh_token() {
    let env = Env::new();
    let fake = Fake::new(true, true);
    fake.set_err("AUTH_REQUIRED");
    let source = source(&env, &fake);
    seed(&source, "fresh-token", i64::MAX);
    let (address, _) = serve(&source, Duration::from_secs(30));
    let (_, response) = exchange(address, "POST /v1/credential/1/refresh HTTP/1.1", "");
    assert!(response.starts_with("HTTP/1.1 401 "), "{response}");
    assert!(!response.contains("fresh-token"));
    let (_, response) = exchange(address, "GET /v1/snapshot?wait=0 HTTP/1.1", "");
    assert!(response.starts_with("HTTP/1.1 401 ") && !response.contains("fresh-token"), "{response}");
    assert!(source.state.lock().access.is_none());
}

#[test]
fn a_waiter_timeout_or_cancel_leaves_the_shared_job_for_other_waiters() {
    let env = Env::new();
    let fake = Fake::new(false, true);
    let source = source(&env, &fake);
    let patient = spawn_call(&source, true, Duration::from_secs(8));
    wait_until("shared job start", || fake.calls() == 1);
    let impatient = Instant::now();
    assert_eq!(code(call(&source, true, Duration::from_millis(150))), "AUTH_REFRESH_TIMEOUT");
    assert!(impatient.elapsed() < Duration::from_secs(2));
    let cancel = Arc::new(AtomicBool::new(false));
    let cancelled = {
        let (source, cancel) = (Arc::clone(&source), Arc::clone(&cancel));
        std::thread::spawn(move || source.ensure(true, Instant::now() + Duration::from_secs(8), &|| cancel.load(Ordering::SeqCst)))
    };
    std::thread::sleep(Duration::from_millis(100));
    cancel.store(true, Ordering::SeqCst);
    assert_eq!(code(cancelled.join().unwrap()), "AUTH_REFRESH_CANCELLED");
    assert_eq!(fake.calls(), 1);
    fake.open();
    patient.join().unwrap().unwrap();
    assert_eq!(fake.calls(), 1);
    // 결과는 캐시에 게시되어 이후 일반 조회는 CLI를 다시 부르지 않는다.
    call(&source, false, Duration::from_secs(1)).unwrap();
    assert_eq!(fake.calls(), 1);
}

#[test]
fn a_hung_refresh_times_out_for_waiters_without_a_second_overlapping_worker() {
    let env = Env::new();
    let fake = Fake::new(false, false);
    let budget = Arc::new(RefreshBudget::new(4));
    let source = source_with(&env, env.account(), &fake, &budget, Duration::from_millis(250), Duration::from_millis(120));
    let started = Instant::now();
    assert_eq!(code(call(&source, true, Duration::from_secs(8))), "AUTH_REFRESH_TIMEOUT");
    assert!(started.elapsed() < Duration::from_secs(3), "waiter hung behind CLI I/O for {:?}", started.elapsed());
    // cooldown 안에서는 같은 실패를 재사용한다.
    assert_eq!(code(call(&source, true, Duration::from_secs(1))), "AUTH_REFRESH_TIMEOUT");
    // cooldown이 지나도 이전 worker가 아직 도는 동안에는 새 worker를 만들지 않는다.
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(code(call(&source, true, Duration::from_secs(1))), "AUTH_REFRESH_BUSY");
    assert_eq!(fake.calls(), 1);
    assert_eq!(budget.active(), 1);
    // 마감 뒤에 도착한 결과는 게시되지 않는다.
    fake.open();
    wait_until("late worker finish", || budget.active() == 0);
    assert!(source.state.lock().access.is_none());
    // worker가 끝난 뒤에는 다음 갱신이 가능하다.
    std::thread::sleep(Duration::from_millis(150));
    call(&source, true, Duration::from_secs(2)).unwrap();
    assert_eq!(fake.calls(), 2);
    assert!(source.state.lock().access.is_some());
}

#[test]
fn a_late_result_is_rejected_after_the_binding_or_enablement_changed() {
    for change in ["identity", "disabled", "binary"] {
        let env = Env::new();
        let fake = Fake::new(false, true);
        let account = env.account();
        let source = source_with(&env, account.clone(), &fake, &Arc::new(RefreshBudget::new(4)), Duration::from_secs(10), Duration::from_millis(50));
        let waiter = spawn_call(&source, true, Duration::from_secs(5));
        wait_until("loader start", || fake.calls() == 1);
        let mut changed = account.clone();
        match change {
            "identity" => changed.identity_key = Some("anthropic|email:a@example.test|workspace:org-b".into()),
            "disabled" => changed.enabled = false,
            _ => changed.binary_path = Some("/elsewhere/claude".into()),
        }
        env.save(&changed);
        fake.open();
        let expected = match change {
            "identity" => "PROFILE_IDENTITY_MISMATCH",
            "disabled" => "ACCOUNT_DISABLED",
            _ => "PROFILE_UNVERIFIED",
        };
        assert_eq!(code(waiter.join().unwrap()), expected, "{change}");
        assert!(source.state.lock().access.is_none(), "{change}: stale token was published");
        // binding이 되돌아오면 cooldown 뒤 다음 갱신이 성공한다.
        env.save(&account);
        std::thread::sleep(Duration::from_millis(120));
        call(&source, true, Duration::from_secs(2)).unwrap();
        assert_eq!(fake.calls(), 2, "{change}");
    }
}

#[test]
fn a_panicking_loader_releases_the_job_permit_and_profile() {
    let env = Env::new();
    let fake = Fake::new(true, true);
    fake.panics.store(true, Ordering::SeqCst);
    let budget = Arc::new(RefreshBudget::new(1));
    let source = source_with(&env, env.account(), &fake, &budget, Duration::from_secs(10), Duration::from_millis(50));
    assert_eq!(code(call(&source, true, Duration::from_secs(3))), "AUTH_REFRESH_UNAVAILABLE");
    assert_eq!(budget.active(), 0);
    fake.panics.store(false, Ordering::SeqCst);
    std::thread::sleep(Duration::from_millis(120));
    call(&source, true, Duration::from_secs(3)).unwrap();
    assert_eq!(fake.calls(), 2);
}

#[test]
fn the_global_budget_bounds_concurrent_jobs_and_returns_the_permit() {
    let env = Env::new();
    let (slow, quick) = (Fake::new(false, true), Fake::new(true, true));
    let budget = Arc::new(RefreshBudget::new(1));
    assert_eq!(budget.limit(), 1);
    let first = source_with(&env, env.account(), &slow, &budget, Duration::from_secs(10), Duration::from_millis(50));
    let second = source_with(&env, env.account(), &quick, &budget, Duration::from_secs(10), Duration::from_millis(50));
    let held = spawn_call(&first, true, Duration::from_secs(5));
    wait_until("permit held", || budget.active() == 1);
    assert_eq!(code(call(&second, true, Duration::from_secs(2))), "AUTH_REFRESH_BUSY");
    assert_eq!(quick.calls(), 0);
    slow.open();
    held.join().unwrap().unwrap();
    assert_eq!(budget.active(), 0);
    // 혼잡 거절은 cooldown에 남지 않아 permit이 돌아오면 바로 실행된다.
    call(&second, true, Duration::from_secs(2)).unwrap();
    assert_eq!(quick.calls(), 1);
}

#[test]
fn a_replacement_source_for_the_same_profile_does_not_overlap_the_running_worker() {
    let env = Env::new();
    let fake = Fake::new(false, true);
    let account = env.account();
    let budget = Arc::new(RefreshBudget::new(4));
    let old = source_with(&env, account.clone(), &fake, &budget, Duration::from_secs(10), Duration::from_millis(50));
    let replacement = source_with(&env, account, &fake, &budget, Duration::from_secs(10), Duration::from_millis(50));
    let running = spawn_call(&old, true, Duration::from_secs(5));
    wait_until("old worker", || fake.calls() == 1);
    assert_eq!(code(call(&replacement, true, Duration::from_secs(2))), "AUTH_REFRESH_BUSY");
    assert_eq!(fake.calls(), 1);
    fake.open();
    running.join().unwrap().unwrap();
    call(&replacement, true, Duration::from_secs(2)).unwrap();
    assert_eq!(fake.calls(), 2);
}

/// 정상 TCP write-half-close(요청을 다 보낸 뒤 FIN)는 취소가 아니다. 응답은 그대로 도착해야 한다.
#[test]
fn a_write_half_closed_client_still_receives_the_shared_refresh() {
    let env = Env::new();
    let fake = Fake::new(false, true);
    let source = source(&env, &fake);
    let (address, _) = serve(&source, Duration::from_secs(30));
    let client = connect(address, "POST /v1/credential/1/refresh HTTP/1.1", "");
    client.shutdown(Shutdown::Write).unwrap();
    wait_until("loader start", || fake.calls() == 1);
    std::thread::sleep(Duration::from_millis(600));
    fake.open();
    let response = read_all(client);
    assert!(response.starts_with("HTTP/1.1 200 "), "{response}");
    assert!(response.contains("\"access\":\"token-1\""));
}

/// RST로 끊긴 요청 대기자는 빨리 떠나고, 공유 job은 영향받지 않고 끝나 결과가 게시된다.
#[cfg(unix)]
#[test]
fn a_reset_client_leaves_quickly_without_cancelling_the_refresh() {
    use std::os::fd::AsRawFd;
    let env = Env::new();
    let fake = Fake::new(false, true);
    let source = source(&env, &fake);
    let (address, done) = serve(&source, Duration::from_secs(30));
    let client = connect(address, "POST /v1/credential/1/refresh HTTP/1.1", "");
    wait_until("loader start", || fake.calls() == 1);
    let linger = libc::linger { l_onoff: 1, l_linger: 0 };
    unsafe {
        libc::setsockopt(
            client.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_LINGER,
            &linger as *const libc::linger as *const libc::c_void,
            std::mem::size_of::<libc::linger>() as libc::socklen_t,
        );
    }
    drop(client);
    done.recv_timeout(Duration::from_secs(3)).expect("handler kept waiting on a reset socket");
    fake.open();
    wait_until("job publishes", || source.state.lock().access.is_some());
    assert_eq!(fake.calls(), 1);
    assert_eq!(source.budget.active(), 0);
}

fn long_poll(address: SocketAddr, wait_ms: u64) -> (Duration, String) {
    exchange(address, &format!("GET /v1/snapshot?wait={wait_ms} HTTP/1.1\r\nIf-None-Match: \"7\""), "")
}

/// omp gateway는 가진 generation으로 `/v1/snapshot?wait=…`를 다시 요청한다. 즉시 답하면 CPU를 태우므로
/// 마감까지 붙들고, 그 사이 상태가 바뀌면 곧바로 깨워야 하며, 요청 마감은 wait보다 우선한다.
#[test]
fn snapshot_long_poll_waits_for_its_deadline_wakes_on_change_and_honors_request_deadline() {
    let env = Env::new();
    let fake = Fake::new(true, true);
    let source = source(&env, &fake);
    seed(&source, "token", i64::MAX);
    let (address, _) = serve(&source, Duration::from_secs(45));
    let (elapsed, response) = long_poll(address, 400);
    assert!(elapsed >= Duration::from_millis(350), "answered after {elapsed:?} instead of waiting");
    assert!(response.contains("ETag: \"7\""), "{response}");
    assert_eq!(fake.calls(), 0, "a fresh token must not start a CLI refresh");

    let poll = std::thread::spawn(move || long_poll(address, 5_000));
    std::thread::sleep(Duration::from_millis(150));
    let (_, response) = exchange(address, "POST /v1/credential/1/block HTTP/1.1", &block_body());
    assert!(response.starts_with("HTTP/1.1 200 "));
    let (elapsed, response) = poll.join().unwrap();
    assert!(elapsed < Duration::from_secs(2), "change did not wake the poll ({elapsed:?})");
    assert!(response.contains("ETag: \"8\""), "{response}");

    let (short, _) = serve(&source, Duration::from_millis(300));
    let started = Instant::now();
    let response = {
        let stream = connect(short, "GET /v1/snapshot?wait=20000 HTTP/1.1\r\nIf-None-Match: \"8\"", "");
        read_all(stream)
    };
    let elapsed = started.elapsed();
    assert!(elapsed >= Duration::from_millis(250) && elapsed < Duration::from_secs(3), "request deadline ignored: {elapsed:?}");
    assert!(response.starts_with("HTTP/1.1 200 "), "{response}");
}

#[test]
fn expired_or_cancelled_requests_do_not_start_an_authentication_job() {
    let env = Env::new();
    let fake = Fake::new(false, true);
    let source = source(&env, &fake);
    assert_eq!(
        source.ensure(true, Instant::now(), &|| false).unwrap_err().code,
        "AUTH_REFRESH_TIMEOUT"
    );
    assert_eq!(
        source.ensure(true, Instant::now() + Duration::from_secs(5), &|| true).unwrap_err().code,
        "AUTH_REFRESH_CANCELLED"
    );
    assert_eq!(fake.calls(), 0);
    assert!(!source.state.lock().running);
    assert_eq!(source.budget.active(), 0);
}

#[test]
fn cached_ready_does_not_bypass_a_disabled_account() {
    let env = Env::new();
    let fake = Fake::new(true, true);
    let source = source(&env, &fake);
    seed(&source, "cached-token", i64::MAX);
    let mut account = source.account.clone();
    account.enabled = false;
    env.save(&account);
    assert_eq!(source.ready().unwrap_err().code, "ACCOUNT_DISABLED");
    assert_eq!(fake.calls(), 0);
}

#[test]
fn a_waiting_snapshot_rechecks_registration_before_publishing_cached_access() {
    let env = Env::new();
    let source = source(&env, &Fake::new(true, true));
    seed(&source, "cached-token", i64::MAX);
    let (address, done) = serve(&source, Duration::from_secs(10));
    let poll = std::thread::spawn(move || long_poll(address, 25_000));
    wait_until("snapshot waiting after initial validation", || source.changed.notify_all() > 0);
    let mut account = source.account.clone();
    account.enabled = false;
    env.save(&account);
    source.state.lock().generation += 1;
    source.changed.notify_all();
    let (_, response) = poll.join().unwrap();
    done.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(response.starts_with("HTTP/1.1 409 "), "{response}");
    assert!(!response.contains("cached-token"));
}

#[test]
fn unconfirmed_authentication_is_not_reported_as_a_confirmed_login_rejection() {
    let env = Env::new();
    let fake = Fake::new(true, true);
    let source = source(&env, &fake);
    seed(&source, "cached-token", i64::MAX);
    let mut account = source.account.clone();
    account.auth_status = "error".into();
    env.save(&account);
    assert_eq!(source.ready().unwrap_err().code, "AUTH_REFRESH_UNAVAILABLE");
    account.auth_status = "auth-required".into();
    env.save(&account);
    assert_eq!(source.ready().unwrap_err().code, "AUTH_REQUIRED");
    assert_eq!(fake.calls(), 0);
}

/// 로그 줄을 모으는 sink. 실패·복구 줄이 몇 번, 어떤 모양으로 남는지 본다.
fn capture() -> (LogSink, Arc<Lock<Vec<String>>>) {
    let lines = Arc::new(Lock::new(Vec::new()));
    let sink = Arc::clone(&lines);
    (Arc::new(move |line: &str| sink.lock().push(line.to_owned())), lines)
}
fn logged_source(env: &Env, fake: &Arc<Fake>, sink: LogSink, cooldown: Duration) -> Arc<Source> {
    let account = env.account();
    env.save(&account);
    Arc::new(
        Source::new(account, "anthropic", IDENTITY.into(), Arc::downgrade(&env.service), Arc::new(RefreshBudget::new(4)))
            .with_log(sink)
            .with_loader(fake.loader(), Duration::from_secs(10), cooldown),
    )
}

#[test]
fn a_shared_failure_keeps_its_exact_code_stage_and_counts_once_then_recovery_is_logged_once() {
    let env = Env::new();
    let fake = Fake::new(false, true);
    fake.set_err("NATIVE_AUTH_UNAVAILABLE");
    fake.set_stage("cli-refresh");
    let (sink, lines) = capture();
    let source = logged_source(&env, &fake, sink, Duration::from_millis(60));
    let waiters: Vec<_> = (0..6).map(|_| spawn_call(&source, true, Duration::from_secs(5))).collect();
    wait_until("loader started", || fake.calls() == 1);
    // 실행 중에는 지금 단계와 경과 시간이 보인다. 아직 실패로 세지 않는다.
    let running = source.auth_status();
    assert_eq!(running.activity.as_ref().map(|activity| activity.stage), Some("cli-refresh"));
    assert_eq!((running.failures, running.state), (0, "unknown"));
    fake.open();
    for waiter in waiters {
        assert_eq!(code(waiter.join().unwrap()), "NATIVE_AUTH_UNAVAILABLE", "waiters must see the exact code, not a generic one");
    }
    let failing = source.auth_status();
    assert_eq!((failing.state, failing.failures, failing.consecutive_failures, failing.recoveries), ("failing", 1, 1, 0));
    assert!(failing.activity.is_none());
    let last = failing.last_failure.as_ref().unwrap();
    assert_eq!((last.code.as_str(), last.stage, last.transient), ("NATIVE_AUTH_UNAVAILABLE", "cli-refresh", true));
    assert_eq!(lines.lock().len(), 1, "one failed job writes one line however many callers shared it: {:?}", lines.lock());
    let line = lines.lock()[0].clone();
    assert!(line.contains("bridge/native session=- outcome=auth-failure"), "{line}");
    assert!(line.contains("code=NATIVE_AUTH_UNAVAILABLE stage=cli-refresh") && line.contains("transient=true"), "{line}");
    assert!(line.contains(&format!("account={}", account_key(IDENTITY))), "{line}");
    assert!(!line.contains("a@example.test") && !line.contains("token-1") && !line.contains("->"), "{line}");

    // 같은 코드가 이어지는 동안은 다시 남기지 않지만 세기는 한다.
    std::thread::sleep(Duration::from_millis(100));
    fake.set_stage("credential-reread");
    assert_eq!(code(call(&source, true, Duration::from_secs(2))), "NATIVE_AUTH_UNAVAILABLE");
    let still = source.auth_status();
    assert_eq!((still.failures, still.consecutive_failures), (2, 2));
    assert_eq!(still.last_failure.as_ref().unwrap().stage, "credential-reread");
    assert_eq!(lines.lock().len(), 1, "an unchanged failure is not logged again");

    // 복구하면 한 번만 남고 연속 실패가 0으로 돌아온다. 다음 실패는 새 사건이라 다시 남는다.
    std::thread::sleep(Duration::from_millis(100));
    fake.set_ok("token-2");
    call(&source, true, Duration::from_secs(2)).unwrap();
    let ok = source.auth_status();
    assert_eq!((ok.state, ok.consecutive_failures, ok.recoveries, ok.failures), ("ok", 0, 1, 2));
    assert!(ok.last_success_at.is_some());
    let all = lines.lock().clone();
    assert_eq!(all.len(), 2, "{all:?}");
    assert!(all[1].contains("outcome=auth-recovered") && all[1].contains("failures=2"), "{}", all[1]);
    assert!(!all[1].contains("token-2"));
}

#[test]
fn a_timed_out_job_reports_the_stage_it_was_stuck_in() {
    let env = Env::new();
    let fake = Fake::new(false, false);
    fake.set_stage("identity-recheck");
    let (sink, lines) = capture();
    let account = env.account();
    env.save(&account);
    let source = Arc::new(
        Source::new(account, "anthropic", IDENTITY.into(), Arc::downgrade(&env.service), Arc::new(RefreshBudget::new(4)))
            .with_log(sink)
            .with_loader(fake.loader(), Duration::from_millis(200), Duration::from_millis(50)),
    );
    assert_eq!(code(call(&source, true, Duration::from_secs(5))), "AUTH_REFRESH_TIMEOUT");
    let status = source.auth_status();
    let last = status.last_failure.unwrap();
    assert_eq!((last.code.as_str(), last.stage, last.transient), ("AUTH_REFRESH_TIMEOUT", "identity-recheck", true));
    assert!(last.duration_ms >= 150, "{}", last.duration_ms);
    assert!(lines.lock()[0].contains("stage=identity-recheck"));
    fake.open();
}

#[test]
fn expired_or_cancelled_refresh_waiters_never_take_an_available_slot() {
    let budget = Arc::new(RefreshBudget::new(1));
    let expired = budget.acquire_until(Instant::now() - Duration::from_millis(1), &|| false);
    assert_eq!(expired.err().unwrap().code, "AUTH_REFRESH_BUSY");
    let cancelled = budget.acquire_until(Instant::now() + Duration::from_secs(1), &|| true);
    assert_eq!(cancelled.err().unwrap().code, "AUTH_REFRESH_CANCELLED");
    assert_eq!((budget.active(), budget.waiting()), (0, 0));
}

#[test]
fn cancellation_after_slot_acquisition_does_not_start_a_refresh_worker() {
    let env = Env::new();
    let fake = Fake::new(true, true);
    let budget = Arc::new(RefreshBudget::new(1));
    let source = source_with(&env, env.account(), &fake, &budget, Duration::from_secs(10), Duration::from_millis(50));
    let outcome = source.ensure(true, Instant::now() + Duration::from_secs(1), &|| budget.active() == 1);
    assert_eq!(code(outcome), "AUTH_REFRESH_CANCELLED");
    assert_eq!(fake.calls(), 0);
    assert_eq!((budget.active(), budget.waiting()), (0, 0));
    call(&source, true, Duration::from_secs(1)).unwrap();
    assert_eq!(fake.calls(), 1, "cancelled startup must release the source starting flag");
}

#[test]
fn permit_waiters_are_served_in_arrival_order_instead_of_failing_busy() {
    let env = Env::new();
    let (slow, quick_a, quick_b) = (Fake::new(false, true), Fake::new(true, true), Fake::new(true, true));
    let budget = Arc::new(RefreshBudget::new(1));
    let first = source_with(&env, env.account(), &slow, &budget, Duration::from_secs(10), Duration::from_millis(50));
    let second = source_with(&env, env.account(), &quick_a, &budget, Duration::from_secs(10), Duration::from_millis(50));
    let third = source_with(&env, env.account(), &quick_b, &budget, Duration::from_secs(10), Duration::from_millis(50));
    let held = spawn_call(&first, true, Duration::from_secs(5));
    wait_until("permit held", || budget.active() == 1);
    let one = spawn_call(&second, true, Duration::from_secs(5));
    wait_until("second queued", || budget.waiting() == 1);
    // 같은 소스의 추가 대기자는 따로 자리를 기다리지 않는다(대기열은 프로필 수를 넘지 않는다).
    let (joined_tx, joined_rx) = mpsc::channel();
    let extra_source = Arc::clone(&second);
    let extra = std::thread::spawn(move || {
        let checks = std::cell::Cell::new(0);
        extra_source.ensure(true, Instant::now() + Duration::from_secs(5), &|| {
            checks.set(checks.get() + 1);
            if checks.get() == 2 { joined_tx.send(()).unwrap(); }
            false
        })
    });
    let two = spawn_call(&third, true, Duration::from_secs(5));
    wait_until("third queued", || budget.waiting() == 2);
    joined_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!((quick_a.calls(), quick_b.calls()), (0, 0), "waiters must not run past the global limit");
    slow.open();
    held.join().unwrap().unwrap();
    one.join().unwrap().unwrap();
    extra.join().unwrap().unwrap();
    two.join().unwrap().unwrap();
    assert_eq!((quick_a.calls(), quick_b.calls()), (1, 1));
    assert_eq!((budget.active(), budget.waiting()), (0, 0));
    assert_eq!(second.auth_status().busy_rejections + third.auth_status().busy_rejections, 0);
}

#[test]
fn a_queued_waiter_observes_a_refresh_that_finishes_before_it_wakes() {
    let env = Env::new();
    let fake = Fake::new(true, true);
    let budget = Arc::new(RefreshBudget::new(1));
    let held = budget.acquire_until(Instant::now() + Duration::from_secs(10), &|| false).unwrap();
    let source = source_with(&env, env.account(), &fake, &budget, Duration::from_secs(10), Duration::from_millis(50));
    let first = spawn_call(&source, true, Duration::from_secs(10));
    wait_until("refresh queued", || budget.waiting() == 1);
    let (waiting_tx, waiting_rx) = mpsc::channel();
    let (resume_tx, resume_rx) = mpsc::channel();
    let delayed_source = Arc::clone(&source);
    let delayed = std::thread::spawn(move || {
        let checks = std::cell::Cell::new(0);
        delayed_source.ensure(true, Instant::now() + Duration::from_secs(10), &|| {
            checks.set(checks.get() + 1);
            if checks.get() == 2 {
                waiting_tx.send(()).unwrap();
                resume_rx.recv_timeout(Duration::from_secs(10)).unwrap();
            }
            false
        })
    });
    waiting_rx.recv_timeout(Duration::from_secs(10)).unwrap();
    drop(held);
    first.join().unwrap().unwrap();
    resume_tx.send(()).unwrap();
    delayed.join().unwrap().unwrap();
    assert_eq!(fake.calls(), 1, "a waiter already queued for this refresh must share its result");
    call(&source, true, Duration::from_secs(5)).unwrap();
    assert_eq!(fake.calls(), 2, "a new forced refresh after completion must still run");
}

#[test]
fn a_permit_wait_that_never_clears_ends_in_busy_without_a_cooldown_and_a_cancelled_waiter_leaves_the_queue() {
    let env = Env::new();
    let (slow, quick) = (Fake::new(false, true), Fake::new(true, true));
    let budget = Arc::new(RefreshBudget::new(1));
    let first = source_with(&env, env.account(), &slow, &budget, Duration::from_secs(10), Duration::from_secs(5));
    let second = source_with(&env, env.account(), &quick, &budget, Duration::from_secs(10), Duration::from_secs(5));
    let held = spawn_call(&first, true, Duration::from_secs(8));
    wait_until("permit held", || budget.active() == 1);
    let started = Instant::now();
    assert_eq!(code(call(&second, true, Duration::from_millis(300))), "AUTH_REFRESH_BUSY");
    assert!(started.elapsed() >= Duration::from_millis(250));
    assert_eq!(budget.waiting(), 0, "a timed-out waiter leaves the queue");
    assert_eq!(second.auth_status().busy_rejections, 1);
    // 혼잡 거절은 실패 cooldown에 남지 않는다: 자리가 나면 바로 실행된다.
    slow.open();
    held.join().unwrap().unwrap();
    call(&second, true, Duration::from_secs(2)).unwrap();
    assert_eq!(quick.calls(), 1);
    assert_eq!(second.auth_status().failures, 0, "congestion is not a CLI failure");
    // 취소: 소켓이 사라진 대기자는 자리를 기다리지 않고 나간다.
    let slow = Fake::new(false, true);
    let third = source_with(&env, env.account(), &slow, &budget, Duration::from_secs(10), Duration::from_secs(5));
    let held = spawn_call(&third, true, Duration::from_secs(8));
    wait_until("permit held again", || budget.active() == 1);
    let gone = Arc::new(AtomicBool::new(false));
    let waiter = {
        let (second, gone) = (Arc::clone(&second), Arc::clone(&gone));
        std::thread::spawn(move || second.ensure(true, Instant::now() + Duration::from_secs(5), &|| gone.load(Ordering::SeqCst)))
    };
    wait_until("cancelled waiter queued", || budget.waiting() == 1);
    gone.store(true, Ordering::SeqCst);
    assert_eq!(code(waiter.join().unwrap()), "AUTH_REFRESH_CANCELLED");
    assert_eq!(budget.waiting(), 0);
    slow.open();
    held.join().unwrap().unwrap();
}

#[test]
fn failure_classes_are_grounded_and_distinct_messages_exist_for_each_code() {
    // 일시 실패(gateway 유지): 어댑터가 deadline·프로세스 I/O·CLI 오류 응답·키체인 잠금으로 내는 코드.
    for code in [
        "AUTH_REFRESH_BUSY", "AUTH_REFRESH_TIMEOUT", "AUTH_REFRESH_CANCELLED", "AUTH_REFRESH_UNAVAILABLE", "PROBE_TIMEOUT",
        "PROBE_START_FAILED", "PROBE_IO", "PROBE_CLOSED", "CLI_INSPECTION_FAILED", "NATIVE_METADATA_UNAVAILABLE",
        "NATIVE_AUTH_UNAVAILABLE", "NATIVE_KEYCHAIN_LOCKED",
    ] {
        assert!(transient_failure(code), "{code}");
        assert!(failure(code).1.contains("다시 로그인할 필요는 없어요") || code == "AUTH_REFRESH_CANCELLED", "{code}");
    }
    // 차단 유지(fail closed): 로그아웃·제외·identity/프로필/인증 덮어쓰기 충돌·형식 변경·바이너리 문제.
    for code in [
        "AUTH_REQUIRED", "AUTH_STATUS_UNCONFIRMED", "ACCOUNT_DISABLED", "PROFILE_IDENTITY_MISMATCH", "AUTH_OVERRIDE_CONFLICT",
        "PROFILE_UNVERIFIED", "PROFILE_UNBOUND", "NATIVE_STORAGE_UNSUPPORTED", "NATIVE_CREDENTIAL_READ", "NATIVE_CREDENTIAL_FORMAT",
        "NATIVE_REFRESH_UNEXPECTED", "PROBE_SCHEMA", "PROBE_OUTPUT_LIMIT", "BINARY_MISSING", "BINARY_INVALID", "SHIM_RECURSION",
        "TOOL_UNSUPPORTED",
    ] {
        assert!(!transient_failure(code), "{code}");
    }
    // 코드마다 일반 문구가 아니라 자기 문구를 쓴다.
    let generic = failure("SOMETHING_NEW").1;
    for code in [
        "NATIVE_AUTH_UNAVAILABLE", "AUTH_STATUS_UNCONFIRMED", "NATIVE_REFRESH_UNEXPECTED", "PROBE_TIMEOUT", "PROBE_START_FAILED",
        "PROBE_IO", "PROBE_SCHEMA", "BINARY_INVALID", "NATIVE_CREDENTIAL_FORMAT",
    ] {
        assert_ne!(failure(code).1, generic, "{code} fell through to the generic message");
    }
    // 로그인 거절이라고 단정하지 않는 코드들은 401이 아니다.
    for code in ["NATIVE_AUTH_UNAVAILABLE", "AUTH_STATUS_UNCONFIRMED", "PROBE_TIMEOUT", "PROBE_START_FAILED"] {
        assert_eq!(failure(code).0, 503, "{code}");
    }
    assert_eq!(failure("AUTH_REQUIRED").0, 401);
    assert!(failure("NATIVE_AUTH_UNAVAILABLE").1.contains("거절됐다고 확인된 것은 아니에요"));
}

#[test]
fn the_http_error_for_a_transient_cli_failure_carries_the_exact_code_without_secrets() {
    let env = Env::new();
    let fake = Fake::new(true, true);
    fake.set_err("PROBE_START_FAILED");
    let source = source(&env, &fake);
    let (address, _) = serve(&source, Duration::from_secs(5));
    let (_, response) = exchange(address, "POST /v1/credential/1/refresh HTTP/1.1", "");
    assert!(response.starts_with("HTTP/1.1 503 "), "{response}");
    assert!(response.contains("\"code\":\"PROBE_START_FAILED\""), "{response}");
    assert!(!response.contains("access") && !response.contains("controlled failure"), "{response}");
}

#[test]
fn a_transient_failure_never_serves_the_previous_token_and_recovers_with_the_new_one() {
    let env = Env::new();
    let fake = Fake::new(true, true);
    let source = source(&env, &fake);
    let (address, _) = serve(&source, Duration::from_secs(5));
    let (_, ok) = exchange(address, "POST /v1/credential/1/refresh HTTP/1.1", "");
    assert!(ok.contains("\"access\":\"token-1\""), "{ok}");
    fake.set_err("PROBE_TIMEOUT");
    let (_, failed) = exchange(address, "POST /v1/credential/1/refresh HTTP/1.1", "");
    assert!(failed.starts_with("HTTP/1.1 503 ") && failed.contains("\"code\":\"PROBE_TIMEOUT\""), "{failed}");
    assert!(!failed.contains("token-1"));
    assert!(source.state.lock().access.is_none(), "the previous token must not survive a failed refresh");
    std::thread::sleep(Duration::from_millis(450));
    fake.set_ok("token-3");
    let (_, again) = exchange(address, "POST /v1/credential/1/refresh HTTP/1.1", "");
    assert!(again.contains("\"access\":\"token-3\""), "{again}");
}
