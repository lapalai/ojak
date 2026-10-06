# 보안 검토 결과 (2026-09-27, 공개 전)

읽기 전용 검토. 각 항목은 file:line 근거가 있는 실제 공격·장애 경로다. 수정 상태는 '조치' 열을 갱신한다.


## SecLauncher

## 검토 범위
crates/launcher (arguments.rs 전체, lib.rs run_cli/prepare_launch/run_inner/claude_exec/passthrough, install.rs 전체, shell.rs 전체, omp_extension.rs 전체, omp_broker.rs 전체, deactivate.rs 전체, main.rs argv 분기, omp_bridge.rs migrate_logins/connect), crates/adapters (process.rs discover/executable/search_dirs/Probe, safety.rs 전체, settings.rs 전체, native.rs profile_env/inspect_at, lib.rs inspect_cli/binding_paths/verify/build_launch_plan/register/enrollment_plan). 교차 확인: protocol/lib.rs Paths::discover·call, service/managed_sessions.rs validate_child, service/server.rs getpeereid, apps/desktop/src-tauri/src/main.rs open_terminal·launch_session·login_account.

## 결론 (한국어 요약)
1. **[high] 서비스 응답의 binary_path 무조건 신뢰 + /tmp 소켓 위장** — run_inner는 lease.acquire가 돌려준 grant.account.binary_path를 `verify_at`(`auth status --json` 프로브)로 즉시 실행하고, IDENTITY_MISMATCH 검사(lib.rs:554)는 같은 grant에서 파생한 두 값을 비교해 항진식이다. 클라이언트 `call()`은 서버 peer uid를 검증하지 않고, 소켓 경로는 `$TMPDIR/aam-<uid>-<fnv>/control.sock`로 결정적이며 TMPDIR이 없으면 `/tmp`로 떨어진다. SSH 세션 등 TMPDIR 없는 컨텍스트에서 다른 로컬 사용자가 소켓을 미리 만들면 피해자 uid로 임의 바이너리가 실행된다. 로컬 integration.json(`registered_native`)과 대조하는 핀과 클라이언트 측 getpeereid 검사가 필요하다.
2. **[medium] 비UTF-8 argv 허용목록 우회** — `to_str()`이 None이면 위치 인수로 취급되어 `--settings=/path\xff`, `--mcp-config=...` 같은 금지 옵션이 원본 CLI에 전달된다(Node/Bun은 U+FFFD 치환 디코딩 [INFERENCE]). 인증 우회 정책·--settings hooks 전용 검증이 모두 무력화된다. 권한 상승은 아니고 회계·정책 우회.
3. **[medium] broker bearer token 평문 전송·상대 미검증** — 127.0.0.1:8765를 점유한 타 사용자에게 `~/.omp/auth-broker.token`과 bridge token이 전달될 수 있음(다중 사용자 Mac 한정).
4. **[low] AAM_CLAUDE_ROOT_PROGRAM** — 구체 평가: validate-child는 읽기 전용이고 exec는 호출자 자신의 uid·env로 이루어져 권한 이득이 없다. 다만 '자식 바이너리 검증'이 같은 트리 프로세스에 대해 자기참조적이 되므로 서비스가 session.program을 저장·반환하는 방식으로 pin하는 것을 권고.
5. **[low] omp config.yml/LaunchAgent 쓰기 위생** — 예측 가능 tmp 이름에 fs::write, 모드 미보존, OMP_NATIVE_BIN 무검증(shell.rs 수준의 안전장치 부재).
6. **[low] LaunchAgent가 설치 셸 PATH를 영구 고정** — 데몬 CLI 탐색이 설치 시점 PATH에 종속; discover의 `shims` 필터는 실제 `bin` 디렉터리와 불일치해 무효.

## 문제 없음으로 확인한 항목
- integration.json: read_owned(uid·symlink·크기)·atomic_write(create_new 0600·rename)·version==2 게이트·recursion guard(executable() 이름/`#!` 검사 + launcher/own inode 비교) 양호. same-uid symlink swap은 위협모델 밖.
- shim 설치: SHIM_CONFLICT 사전검사, 롤백은 이번 호출이 바꾼 링크만 복원. LaunchAgent plist는 xml() 이스케이프, 기록 내용 일치 시만 교체·삭제.
- shell.rs: O_NOFOLLOW·uid·nlink==1·부모 디렉터리 0o022·canonical 일치·compare-then-rename, shell_quote는 `'\''` 방식으로 zsh에 안전, 블록 패턴은 quoted literal.
- omp_extension.rs: 조상 symlink 거부, 0700 DirBuilder::create(비재귀) 충돌 실패, hard_link 게시(기존 파일 덮어쓰기 불가), receipt sha256 대조, uninstall은 staging rename 후 재검증.
- validate_native_args(UTF-8 입력 한정): 값 삼킴 방지(`-`로 시작하는 값 거부), inline `=` 불리언 거부, `--` 이후 위치 인수, 첫 위치 인수 하위명령 차단, --settings inline JSON은 hooks/known_host_hook만 허용. 우회 경로 발견 없음.
- safety.rs: env 접두사·키워드 검사와 cwd 조상 `.claude/settings.json` 검사가 AUTH_OVERRIDE_CONFLICT로 중단하며 이 코드는 passthrough 목록에 없어 악성 저장소가 강제 우회를 만들 수 없음.
- settings.rs import: 허용목록 기반, 민감 문자열 필터, create_new 0600.
- Tauri open_terminal: 모든 인수 `'"'"'` 인용, 스크립트 0700 create_new, 자기 삭제. 인젝션 없음.
- deactivate.rs: 서비스 permit 획득 후 단계 실행, 실패 시 재개. 자체 위험 없음.

### [high] 서비스 응답의 account.binary_path를 로컬 기록과 대조 없이 실행 — /tmp 소켓 위장 시 타 사용자 코드 실행
- 위치: `crates/launcher/src/lib.rs:479`; `crates/launcher/src/lib.rs:511`; `crates/launcher/src/lib.rs:553`; `crates/launcher/src/lib.rs:569`
- 요약: run_inner는 lease.acquire 응답(grant.account)에 담긴 binary_path·profile_path를 그대로 build_launch_plan→verify_at에 넘기고, verify_at은 그 경로를 `auth status --json` 프로브로 즉시 실행한다(native.rs:192). 'IDENTITY_MISMATCH' 검사(lib.rs:554)는 같은 grant에서 파생한 program과 bound를 비교하므로 서비스가 거짓말하면 항상 통과한다(항진식). 소켓 클라이언트 `call()`(protocol/lib.rs:459)은 서버 peer uid를 검증하지 않고, 소켓 경로는 `$TMPDIR/aam-<uid>-<fnv(AAM_HOME)>/control.sock`로 결정적이며 TMPDIR이 없으면 `/tmp`(world-writable, sticky)로 떨어진다(protocol/lib.rs:55-57). macOS SSH 세션·cron 등 TMPDIR이 비어 있는 컨텍스트에서 피해자가 `claude` shim을 실행하면 다른 로컬 사용자가 미리 만든 `/tmp/aam-501-<hash>/control.sock`에 접속하고, 공격자 서비스가 `binary_path=/tmp/x/claude`를 담은 PREPARED grant를 돌려주면 피해자 uid로 공격자 바이너리가 실행된다. 서버 쪽만 getpeereid로 클라이언트를 검증(server.rs:76)하고 반대 방향은 없다. [INFERENCE] macOS sshd가 TMPDIR을 설정하지 않는다는 점과 Rust `env::temp_dir()`의 `/tmp` fallback은 문서·경험 기반이며 이 세션에서 실측하지 않았다.
- 권고: 1) protocol::call에서 connect 직후 getpeereid(stream)로 서버 uid == geteuid()를 확인하고 아니면 DAEMON_UNAVAILABLE이 아닌 별도 오류(SOCKET_UNTRUSTED)로 중단(passthrough 목록에 넣지 않음). 소켓 상위 디렉터리가 본인 소유·0700이 아니면 접속하지 않기. `/tmp` fallback 대신 confstr(_CS_DARWIN_USER_TEMP_DIR) 또는 AAM_HOME 하위 고정 경로 사용. 2) run_inner에서 program을 `install::registered_native(paths, &intent.tool)`(uid 검증된 로컬 integration.json) 결과와 비교하고, 불일치 시 verify(프로브)조차 실행하지 않기. 3) grant.account.profile_path도 paths.profiles 하위 또는 ~/.claude 등 허용 루트로 제한.
- 조치: 수정(2026-09-27): 클라이언트 getpeereid 검사(protocol `peer_is_self`), 소켓을 AAM_HOME/run(0700)으로 이동, 실행 파일을 `registered_native`(integration.json)로 고정

### [medium] 비UTF-8 argv는 옵션 검사에서 위치 인수로 취급되어 --settings/--mcp-config 등 금지 옵션이 원본 CLI에 전달됨
- 위치: `crates/launcher/src/arguments.rs:76`; `crates/launcher/src/arguments.rs:108`; `crates/launcher/src/arguments.rs:138`; `crates/launcher/src/arguments.rs:203`
- 요약: validate_native_args는 `arg.to_str()`이 None이면(UTF-8이 아닌 바이트 포함) `starts_with('-')` 검사가 false가 되어 else 분기(위치 인수)로 빠진다(arguments.rs:76-77, 108-113). shim_resume/shim_session_id도 `unwrap_or("")`로 같은 인수를 그대로 native에 넘긴다(138, 203). 따라서 `--settings=/tmp/s.json\xff` 같은 인수는 검사를 통과하고, Node/Bun 런타임은 argv를 U+FFFD 치환으로 디코딩하므로 Claude는 `--settings` 옵션 값 `/tmp/s.json\uFFFD`를 받아 `/tmp/s.json\xef\xbf\xbd` 파일을 읽는다. 호출자가 그 이름의 파일에 `{"env":{"ANTHROPIC_API_KEY":...}}` 또는 `apiKeyHelper`를 두면 관리 세션(lease·quota는 배정 계정에 기록)이 다른 자격증명으로 동작해 '인증 우회 경로를 만들지 않는다' 불변식과 --settings 인라인 hooks 전용 검증(validate_host_settings)이 모두 우회된다. `--mcp-config=<path>\xff`, `--add-dir`, `--setting-sources` 등 금지 옵션 전부에 동일 적용. 공격 주체는 shim에 인수를 넘길 수 있는 호스트·중첩 에이전트·사용자 본인이므로 권한 상승은 아니고 회계·정책 우회다. [INFERENCE] Node/Bun의 U+FFFD 치환 디코딩은 런타임 문서 기반이며 이 세션에서 실측하지 않았다.
- 권고: validate_native_args(및 shim_model/shim_resume/shim_session_id) 진입 시 `--` 이전의 모든 인수가 `to_str()`에 실패하면 AUTH_OVERRIDE_CONFLICT(또는 INVALID_ARGUMENT)로 거부. 최소한 첫 바이트가 b'-'인 비UTF-8 인수는 옵션으로 취급해 거부. 회귀 테스트: `OsString::from_vec(b"--settings=/x\xff".to_vec())` 입력이 Err이어야 함.
- 조치: 수정(2026-09-27): 비UTF-8 인수 거부 + 테스트

### [medium] broker bearer token·bridge token을 127.0.0.1:8765의 검증되지 않은 상대에게 평문 HTTP로 전송
- 위치: `crates/launcher/src/omp_broker.rs:114`; `crates/launcher/src/omp_broker.rs:149`; `crates/launcher/src/omp_broker.rs:297`; `crates/launcher/src/omp_bridge.rs:263`
- 요약: broker_request(omp_broker.rs:114-121)는 `~/.omp/auth-broker.token`을 읽어 `Authorization: Bearer`로 127.0.0.1:8765에 평문 전송하며 상대 프로세스의 소유자를 확인하지 않는다. ensure_broker는 LaunchAgent bootstrap 후 `authenticated_snapshot().is_ok()`(200 + `{"credentials":[]}`면 충분)를 준비 완료로 간주한다(298). omp_bridge::migrate_logins는 upload_credential로 Ojak bridge token을 'oauth' 자격증명으로 같은 포트에 POST한다(omp_bridge.rs:263). 8765는 비특권 포트라 다른 로컬 사용자가 broker가 내려간 순간(첫 connect 전, 재부팅 직후 등) 먼저 bind하면 피해자의 broker token(→ 실제 broker 기동 후 /v1/snapshot으로 전체 공급자 OAuth 자격증명 열람)과 bridge token(→ 피해자 계정으로 과금되는 bridge gateway 사용)을 획득한다. 이 설계는 omp broker 프로토콜에서 상속됐지만 Ojak이 능동적으로 비밀을 업로드하는 경로를 추가했다. 단일 사용자 Mac에서는 영향 없음.
- 권고: 연결 후 상대 소유자 확인: libproc(proc_pidinfo/proc_listpidspath) 또는 `lsof -nP -iTCP:8765 -sTCP:LISTEN -F` 파싱으로 리스너 pid의 uid == geteuid()를 확인하고 아니면 BROKER_UNTRUSTED로 중단(토큰 전송 전). 장기적으로 omp 측에 Unix 소켓 broker 옵션을 요청하고 Ojak은 그것을 우선 사용. bridge token 업로드(migrate_logins)는 리스너 검증 후에만 수행.
- 조치: 미정

### [low] AAM_CLAUDE_ROOT_PROGRAM 환경변수 신뢰로 자식 self-exec 바이너리 검사가 자기참조적 — 권한 이득은 없음(hardening)
- 위치: `crates/launcher/src/lib.rs:697`; `crates/launcher/src/lib.rs:748`; `crates/launcher/src/lib.rs:729`; `crates/launcher/src/install.rs:141`
- 요약: claude_exec는 요청 바이너리가 `bound`(계정 binary_path 해석) 또는 `root`(env AAM_CLAUDE_ROOT_PROGRAM 해석) 중 하나와 같으면 허용한다(lib.rs:697-699, 748-749). root는 환경변수라 관리 Claude 트리 안의 어떤 프로세스(Bash 도구, MCP 서버, hook)도 `AAM_CLAUDE_ROOT_PROGRAM=/tmp/x aam --claude-process-wrapper /tmp/x ...`로 임의 실행 파일을 통과시킬 수 있다. install::native_program은 launcher 자신과의 inode 일치만 거부하고 process::executable의 aam-*/shims/`#!` 재귀 휴리스틱은 적용하지 않는다(install.rs:141). 구체 평가: lease.validate-child는 읽기 전용(managed_sessions.rs:29-59, 세션·계정 반환만)이고 exec는 호출자 자신의 uid·env로 이루어지므로 호출자가 직접 /tmp/x를 실행하는 것과 권한상 차이가 없다. 얻는 것은 (a) '검증된 self-exec'이라는 외형, (b) --claude-native-exec 모드에서 CMUX_CLAUDE_PID가 임의 바이너리를 가리키게 되는 것뿐이다. 따라서 보안 경계 위반은 아니지만 '자식 바이너리 검증'이라는 통제가 실질적으로 bound 비교 하나로 축소된다. 부모(run_inner)는 plan.env로 값을 덮어쓰므로(lib.rs:563) 외부에서 주입은 불가.
- 권고: 세션 시작 시 lease.starting 요청에 program(canonical)을 포함시켜 서비스가 session.program으로 저장하고, validate-child 응답에서 그것을 돌려받아 env 대신 비교. 또는 claude_exec에서 requested에도 process::executable 수준의 재귀 휴리스틱 적용.
- 조치: 완화(2026-09-27): 등록된 원본 CLI와 같은 설치 폴더일 때만 인정

### [low] omp config.yml·LaunchAgent 재작성이 shell.rs 수준의 안전장치 없이 이루어짐 (예측 가능한 임시파일, 모드 미보존, OMP_NATIVE_BIN 무검증)
- 위치: `crates/launcher/src/omp_broker.rs:80`; `crates/launcher/src/omp_broker.rs:233`
- 요약: omp_broker::atomic_write(80-88)는 `.config.yml.aam-<pid>`라는 예측 가능한 임시 이름에 `fs::write`(O_EXCL 없음, symlink 추적, umask 기본 0644)로 쓰고 rename한다. 원본 config.yml의 mode(사용자가 0600으로 둔 경우)·소유자·nlink를 확인하지 않아 shell.rs(read_config O_NOFOLLOW·uid·nlink·mode 보존)와 대비된다. omp config.yml에 공급자 API 키가 들어갈 수 있으므로 0600→0644 완화가 발생할 수 있다. 또한 agent_plist(233-234)는 OMP_NATIVE_BIN 환경변수를 `is_absolute()`만 확인해 KeepAlive LaunchAgent ProgramArguments로 영구 등록한다(파일 존재·실행 비트·소유자 미확인). 모두 same-uid 전제라 경계 위반은 아니며, `~/.omp`가 0700이면 실질 노출은 제한된다.
- 권고: install.rs::atomic_write(new_id() 임시명, create_new, 0600)와 shell.rs::read_config 패턴을 재사용하고 원본 mode를 보존. OMP_NATIVE_BIN은 install::native_program으로 검증(존재·실행·자기 아님)하고 소유자 uid 확인 후 사용.
- 조치: 수정(2026-09-28): 예측 불가 임시 이름·create_new·0600, config.yml 원본 모드 보존, symlink 거부. OMP_NATIVE_BIN은 절대 경로이고 실행 가능한 파일만 허용

### [low] service install이 설치 셸의 전체 PATH를 LaunchAgent에 고정 — 데몬의 CLI 탐색 경로가 설치 시점 환경에 영구 종속
- 위치: `crates/launcher/src/install.rs:612`; `crates/adapters/src/process.rs:20`; `crates/adapters/src/process.rs:141`; `crates/launcher/src/install.rs:301`
- 요약: service_install(install.rs:612-614)은 `aam service install`을 실행한 프로세스의 PATH 전체를 plist EnvironmentVariables.PATH로 기록한다. aam-service는 이 PATH를 process::search_dirs()(process.rs:20-27)의 최우선 탐색 목록으로 사용해 claude/codex를 찾고(integration.json이 없거나 version≠2일 때), integration_install은 서비스가 관측한 경로(observed)를 최우선 후보로 integration.json에 고정한다(install.rs:301-306). 설치 시 PATH에 프로젝트 로컬 디렉터리(node_modules/.bin, venv/bin, direnv가 추가한 경로 등)가 있었다면 그 경로가 데몬 재시작 후에도 계속 탐색되어, 이후 그 디렉터리에 놓인 `claude` 파일이 원본 CLI로 채택될 수 있다. process::discover의 `shims` 필터(process.rs:141)는 실제 shim 디렉터리 `bin`과 이름이 달라 무효하며 재귀 방지는 executable()의 이름 검사에만 의존한다. same-uid 전제의 hardening 항목.
- 권고: 데몬 plist PATH를 고정 목록(/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin + $HOME/.local/bin)으로 제한하고, search_dirs의 PATH 항목 중 HOME 하위·시스템 표준 외 경로는 탐색에서 제외 또는 소유자·mode(0o022) 검사. process.rs:141 필터를 paths.home.join("bin")으로 수정.
- 조치: 수정(2026-09-28): LaunchAgent PATH를 ~/.local/bin:~/.bun/bin:~/.cargo/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin 으로 고정. nvm은 search_dirs가 HOME에서 추가


## SecCore

## 검토 범위
`crates/service`(server.rs, lib.rs dispatch/lease, store.rs, diagnostics.rs, bridge.rs 전체, process.rs, scheduler.rs canonical_directory)와 `crates/protocol/src/lib.rs`(Paths, 프레이밍, call)를 직접 읽었고, 로그 소비자 확인을 위해 `apps/desktop/src-tauri/src/main.rs`의 bridge.log 파서와 `crates/launcher/src/install.rs`의 LaunchAgent plist 생성부, launcher의 grant 검증부를 참조했습니다.

## 결론 (핵심)
- **가장 큰 문제는 브릿지(127.0.0.1:4020)의 인증 전 자원 소모**입니다. 연결 수 제한·전체 요청 데드라인 없이 연결마다 스레드를 만들고, 토큰 검사 전에 최대 64 MiB 본문을 전부 읽습니다. 브릿지가 lease RPC 서비스와 같은 프로세스라서 fd/스레드 고갈이 `server.rs:154`의 accept 오류 → 프로세스 종료, 또는 `bridge.rs:415` 스레드 생성 panic → 브릿지 영구 정지로 이어집니다(로컬 임의 uid 프로세스로 재현 가능, 브라우저는 [INFERENCE]). → medium.
- 브릿지는 loopback 포트 점유자를 검증하지 않고 broker 토큰(:8765)과 gateway 토큰+프롬프트(:41xx)를 보냅니다. 다중 사용자 Mac에서만 의미 있는 경로라 low.
- 인증된 클라이언트 한정으로 upstream 헤더 LF 주입, bridge.log에 이메일 기록(레포 규칙 위반)과 model 문자열 무검증 기록, sticky 세션 무한 증가가 있습니다(low/informational).
- protocol `Paths::discover`는 TMPDIR 미설정(SSH/cron) 시 world-writable `/tmp` 아래 예측 가능한 소켓 경로를 쓰고, 클라이언트 `call()`은 소켓 소유자를 확인하지 않습니다(서버 쪽은 확인함). low.

## 문제 없음으로 확인한 통제
- Unix socket: `getpeereid`로 peer uid == euid 검사(server.rs:70-77), 64 연결 상한(159), 10초 read timeout, 프레임 1 MiB 상한(protocol lib.rs:439-441), `deny_unknown_fields` 파라미터 파싱.
- AAM_HOME/DB/WAL/SHM: `symlink_metadata` + `O_NOFOLLOW` + uid/nlink 검사(server.rs:28-68), 소켓 경로에 다른 파일이 있으면 삭제하지 않음(129-140), flock 단일 인스턴스.
- 브릿지 토큰: `create_new`+0600 생성(bridge.rs:112), 상수 시간 비교(1200-1202), Authorization 헤더는 gateway로 전달하지 않음(1116).
- capability 상수 시간 비교(lib.rs:201-217), SQL은 전부 바인딩 파라미터(store.rs), `trusted_schema=OFF`.
- diagnostics.export는 허용 필드만 재조립하고 계정/세션을 별칭으로 바꾸며 `redact:false`를 거부(diagnostics.rs:52-63); 데스크톱 export도 이 RPC 결과만 저장(main.rs:469). 로그 파일은 내보내지 않음.
- broker snapshot은 provider/identityKey만 추출하고 토큰은 보관하지 않음(bridge.rs:899-928). gateway pool 파일은 포트 번호 기반 이름(경로 조작 불가), 0600.
- 오류 메시지/RPC 응답에 토큰·프롬프트 없음. `Status`의 이메일은 UI 표시용(같은 uid 한정).

### [medium] 브릿지 HTTP 리스너: 인증 전 무제한 연결·스레드·64 MiB 본문 버퍼링으로 aam-service 전체 DoS
- 위치: `crates/service/src/bridge.rs:409`; `crates/service/src/bridge.rs:565`; `crates/service/src/bridge.rs:999`; `crates/service/src/bridge.rs:1106`
- 요약: 127.0.0.1:4020 브릿지는 연결마다 `std::thread::spawn`을 호출하고(연결 상한 없음), 요청당 전체 데드라인 없이 read 호출마다 60초 타임아웃만 두며, Bearer 토큰을 검사하기 **전에** `read_request`가 Content-Length 최대 64 MiB 본문을 전부 메모리에 읽습니다(`/healthz`도 동일). 로컬의 어떤 uid 프로세스든(샌드박스 앱 포함, PNA를 강제하지 않는 브라우저의 웹페이지도 [INFERENCE]) 수백 개 연결을 열어 1바이트씩 흘리거나 64 MiB 본문을 보내면 fd/스레드/메모리가 고갈됩니다. 브릿지가 lease RPC 서비스와 같은 프로세스이므로 (1) Unix socket accept가 EMFILE을 돌려주면 `server.rs:154`가 `Err`를 반환해 프로세스가 종료되고(KeepAlive 재시작 → PREPARED lease ABORTED, 재시작 반복 가능), (2) 스레드 생성 실패 시 `bridge.rs:415`의 `std::thread::spawn`이 panic하여 accept 스레드가 죽고 `listening`은 true로 남아 서비스 재시작 전까지 브릿지가 영구 정지되며, (3) `forward()`가 만든 upstream 소켓에는 read timeout이 없어 gateway가 멈추면 스레드가 무기한 잠깁니다. launchd 에이전트의 기본 soft fd 한도(256, [INFERENCE])라면 200여 개 연결로 충분합니다.
- 권고: (1) 브릿지에도 server.rs:159와 같은 동시 연결 상한(예: 32)과 요청 전체 데드라인(헤더 5초, 본문 30초)을 두고, 초과 시 즉시 끊기. (2) 헤더를 읽은 직후 Authorization을 먼저 검사하고, 인증 실패/`/healthz`/GET 요청은 본문을 읽지 않고(또는 수 KiB만 허용) 응답. (3) `std::thread::Builder::spawn`으로 실패를 처리하고 accept 루프 종료 시 `listening`을 false로 되돌려 재bind. (4) `forward()`가 만든 upstream 스트림에 read/write timeout 설정. (5) LaunchAgent plist에 `SoftResourceLimits/NumberOfFiles`를 올리거나 시작 시 `setrlimit`. (6) 선택: Host 헤더가 `127.0.0.1:4020`/`localhost:4020`이 아니면 거부해 DNS rebinding 프로빙 차단.
- 조치: 수정(2026-09-27): 헤더→인증→본문 순서, 연결 상한 64(초과 503), 요청 30초 제한, spawn 실패 시 해당 연결만 거절, upstream 읽기 300초 제한. 실측: 무인증 64MiB 즉시 401, 120 연결 중 초과분 503, 서비스 생존

### [low] loopback 포트 점유자 미검증: broker 토큰·gateway 토큰·프롬프트를 127.0.0.1:8765/41xx에 바인드된 임의 프로세스로 전송
- 위치: `crates/service/src/bridge.rs:899`; `crates/service/src/bridge.rs:1178`; `crates/service/src/bridge.rs:838`; `crates/service/src/bridge.rs:483`
- 요약: 브릿지는 60초마다 `~/.omp/auth-broker.token`을 Bearer로 붙여 127.0.0.1:8765에 GET하고, 같은 토큰을 gateway 자식 env에 넘기며, `~/.omp/auth-gateway.token`과 사용자 프롬프트 전체를 127.0.0.1:4101–4199로 전달합니다. TCP loopback은 호스트의 모든 uid가 공유하는 이름공간인데 상대가 누구인지(uid, 우리 자식인지) 확인하지 않습니다. `port_free()`는 connect 프로브일 뿐이고 자식이 실제로 bind할 때까지 TOCTOU 창이 있으며, 자식이 살아 있는지(`running()`)만 보고 그 자식이 포트를 실제 보유하는지는 보지 않습니다. 다중 사용자 Mac에서 다른 로컬 사용자가 8765를 먼저 점유하면(피해자 로그인 전 fast user switching 등) 피해자의 broker 토큰을 획득하고, 이후 정상 broker가 뜨면 그 토큰으로 broker의 OAuth 자격증명 스냅샷을 읽을 수 있습니다([INFERENCE]: snapshot이 credential 본문을 포함한다는 것은 `walk()`가 `credential.type`을 찾는 코드에서 추정). 41xx 점유는 해당 identity gateway를 재시작 전까지 영구 비활성화(포트 재배정 없음)하고, 좁은 경합 창에서 gateway 토큰+프롬프트 탈취가 가능합니다. broker 포트 고정은 omp 설계에서 물려받은 것이나, Ojak은 사용자가 omp를 쓰지 않을 때도 주기적으로 토큰을 보내 노출 빈도를 늘립니다.
- 권고: gateway는 `--bind=127.0.0.1:0`(가능하다면)으로 띄워 실제 포트를 자식 출력에서 읽거나, 최소한 spawn 직후 libproc(`proc_pidinfo`/`proc_pidfdinfo`)로 그 포트의 listening 소켓 소유 pid가 우리 자식인지 확인한 뒤에만 healthy로 표시. broker/gateway에 요청 전 nonce 기반 health 핸드셰이크(예: 우리가 생성한 per-gateway secret을 env로 넘기고 응답 헤더로 확인) 도입. 점유된 41xx 포트는 다음 sync에서 다른 포트로 재배정. 장기적으로 omp 쪽에 Unix socket 또는 peer-uid 검증 옵션을 요청.
- 조치: 미정

### [low] 클라이언트 헤더 값의 bare LF/제어문자를 검증 없이 upstream gateway 요청에 재삽입
- 위치: `crates/service/src/bridge.rs:1016`; `crates/service/src/bridge.rs:1113`
- 요약: `read_request`는 헤더 블록을 `\r\n`으로만 나누고 `split_once(':')` 후 trim만 하므로, 값 안의 단독 `\n`(또는 다른 제어문자)은 그대로 남습니다. `forward()`는 content-type/accept/user-agent/x-omp-* 헤더를 `format!("{key}: {value}\r\n")`로 재조립해 gateway에 보내므로, `X-Omp-A: v\nAuthorization: Bearer x` 같은 값이 gateway에서 별도 헤더로 해석될 수 있습니다(gateway HTTP 파서가 bare LF를 줄 끝으로 받아들이는지는 [INFERENCE]; Node/llhttp 버전에 따라 다름). 브릿지 토큰을 가진 클라이언트(=같은 사용자)만 가능하고 gateway 토큰은 브릿지가 이미 붙여 주므로 권한 상승은 없으나, gateway 측 헤더 위조·요청 스머글링 시도 표면입니다.
- 권고: 헤더 키는 RFC 7230 token 문자만, 값은 `\r`/`\n`/기타 제어문자(탭 제외)가 없을 때만 전달하고 위반 시 400. 가능하면 user-agent/accept는 고정값으로 대체하고 x-omp-*만 엄격 검증 후 전달.
- 조치: 수정(2026-09-27): 헤더 값의 제어 문자 제거

### [low] logs/bridge.log에 계정 이메일·조직 기록(레포 규칙 위반) 및 클라이언트 model 문자열 무검증 기록
- 위치: `crates/service/src/bridge.rs:749`; `crates/service/src/bridge.rs:284`; `crates/service/src/bridge.rs:783`; `crates/service/src/bridge.rs:822`
- 요약: CLAUDE.md는 '비밀(token, 이메일, 프롬프트)을 로그·오류·진단 내보내기에 넣지 않는다'고 명시하지만, 성공한 모든 요청에서 `candidate.key`(= `provider|email:<email>|org:<org>`, broker identityKey 그대로)가 `logs/bridge.log`에 기록되고 데스크톱 파서가 `email:` 접두어에 의존합니다(apps/desktop/src-tauri/src/main.rs:604-636). 파일은 0600이고 AAM_HOME이 0700이라 다른 uid에는 노출되지 않으나 Time Machine 백업·지원용 수동 로그 공유 시 계정 이메일이 함께 나갑니다. 같은 줄에 클라이언트 본문의 `model` 문자열이 검증 없이 들어가므로 `\n`을 포함한 model로 위조 줄을 삽입해 데스크톱 사용량 통계를 오염시킬 수 있습니다(토큰 보유 = 같은 사용자라 영향은 낮음).
- 권고: 로그에는 gateway 포트나 `sha256(identityKey)[..8]` 같은 별칭을 쓰고, 데스크톱은 `bridge.status`의 gateway 목록으로 별칭→이메일을 매핑. `model`/`provider`는 `[A-Za-z0-9._:/-]`로 제한하거나 로그 기록 전 제어문자를 제거. main.rs 파서를 같은 커밋에서 갱신.
- 조치: 수정(2026-09-28): 로그 계정 칸은 identity sha256 앞 12 hex. model은 제어 문자·공백 제거 후 128자. bridge.status.accountKey로 데스크톱이 이메일에 매핑

### [informational] gateway 자식 프로세스: broker 토큰 env 전달, stdout/stderr 로그 기본 권한·무제한 성장
- 위치: `crates/service/src/bridge.rs:968`; `crates/service/src/bridge.rs:954`; `crates/service/src/bridge.rs:783`
- 요약: `spawn_gateway`는 `OMP_AUTH_BROKER_TOKEN`을 환경변수로 넘깁니다(같은 uid 프로세스가 `ps -E`/KERN_PROCARGS2로 읽을 수 있음, 다른 uid는 불가). gateway의 stdout/stderr는 `logs/bridge-gateway-<port>.log`에 `.mode(0o600)` 없이(umask 기준 0644) 붙이며, bridge.log와 달리 4 MiB 회전이 없어 gateway가 반복 실패(10초 backoff)하면 무한히 자랍니다. 로그 내용은 omp 바이너리가 결정하므로 토큰/프롬프트 포함 여부는 [INFERENCE]로 확인 불가. AAM_HOME 0700이 상위에서 막아 실질 노출은 같은 사용자에 한정됩니다.
- 권고: 로그 파일을 `.mode(0o600)`으로 열고 spawn 전에 크기 확인 후 `.log.1`로 회전. omp가 `OMP_AUTH_BROKER_TOKEN_FILE` 같은 파일 기반 옵션을 지원하면 그것을 사용.
- 조치: 수정(2026-09-28): gateway 로그를 0600으로 열고 4 MiB에서 .log.1로 회전. broker 토큰 env 전달은 omp 옵션이 없어 유지

### [low] TMPDIR 미설정 시 world-writable /tmp 아래 예측 가능한 소켓 경로 사용, 클라이언트는 소켓 소유자 미검증
- 위치: `crates/protocol/src/lib.rs:55`; `crates/protocol/src/lib.rs:65`; `crates/protocol/src/lib.rs:459`; `crates/service/src/server.rs:28`
- 요약: `Paths::discover`는 `std::env::temp_dir()`(TMPDIR, 없으면 `/tmp`) 아래 `aam-<euid>-<fnv(AAM_HOME)>/control.sock`를 씁니다. AAM_HOME 기본값은 사용자명만으로 예측 가능하므로 경로 전체가 예측됩니다. launchd가 띄우는 서비스·앱은 TMPDIR이 per-user 0700 디렉터리라 안전하지만, SSH/cron/`env -i`처럼 TMPDIR이 없는 셸에서 `aam`/shim을 쓰면 `/tmp/aam-501-<hash>/control.sock`로 연결합니다. 서버 쪽 `private_directory`는 타 uid 소유 디렉터리를 거부하지만(그래서 foreground 서비스 시작 DoS만 가능), 클라이언트 `call()`과 `Paths::prepare`는 디렉터리/소켓 소유자·모드·symlink를 전혀 검사하지 않고 연결합니다. 다른 로컬 사용자가 그 경로를 미리 만들고 자기 소켓을 두면 피해자의 CLI가 공격자 프로세스에 RPC를 보내고 응답(`LeaseGrant`, snapshot)을 신뢰합니다. launcher가 grant의 binary_path/identity를 추가 검증하므로(launcher lib.rs:491-575) 코드 실행까지 이어지는지는 [INFERENCE]이며, 최소한 요청 내용(cwd, 모델, 세션 ID) 노출과 잘못된 계정 배정 유도가 가능합니다.
- 권고: 소켓 디렉터리를 `confstr(_CS_DARWIN_USER_TEMP_DIR)` 또는 AAM_HOME(0700) 아래로 고정해 /tmp 폴백을 없애고(경로 길이 104 제한 주의), `call()`에서 connect 직후 `getpeereid`로 서버 uid == 자기 euid를 확인한 뒤에만 요청을 보내며, `prepare`도 서버의 `private_directory`와 같은 symlink/소유자 검사를 수행.
- 조치: 수정(2026-09-27): 소켓 경로를 AAM_HOME/run으로 이동, 클라이언트 peer uid 검증

### [informational] 클라이언트가 정한 sessionId로 sticky 세션 맵 무제한 증가 → bridge.status 응답이 MAX_FRAME 초과 가능
- 위치: `crates/service/src/bridge.rs:686`; `crates/service/src/bridge.rs:429`; `crates/service/src/bridge.rs:527`; `crates/protocol/src/lib.rs:449`
- 요약: turn 요청마다 본문의 `options.sessionId`(길이·형식 제한 없음)를 키로 `state.sessions`에 항목을 만들고 24시간 보관합니다. 브릿지 토큰을 가진 클라이언트가 새 sessionId를 반복 보내면 메모리가 계속 늘고, `status()`는 최근 60분 세션 전부를 직렬화하므로 `bridge.status` RPC 응답이 1 MiB `MAX_FRAME`을 넘어 `write_frame`이 실패해 데스크톱 연결 화면이 깨집니다. 같은 사용자만 가능하므로 informational.
- 권고: sessionId를 UUID 형식(≤64자)으로 제한하고 sessions 맵을 LRU로 상한(예: 512)을 두며, status()는 최근 N개만 반환.
- 조치: 수정(2026-09-28): sticky 세션 512개 상한, lastUsedAt이 오래된 항목부터 제거. 128자 넘는 sessionId는 저장하지 않음


## SecDesktop

[검토 범위] apps/desktop/src-tauri/src/main.rs 전체(rpc 허용목록, run_management, open_terminal/shell_quote, launch_session/login_account 입력 검증, settings_preview, export_diagnostics, bridge_usage 로그 파서, ui-settings.json, 트레이/팝오버 창 생성, 종료 흐름), tauri.conf.json, capabilities/default.json, entitlements.plist, Cargo.toml, build.rs, apps/desktop/src (api.ts, main.tsx, App.tsx, dialogs.tsx, ConnectionsView/PolicyView/SessionsView/UsageView/Popover/components/state/i18n), integrations/omp/aam-accounts.js, aam-observer.js. 교차 확인용으로 crates/protocol Paths::discover/prepare, service register 파라미터, launcher Run clap 정의, omp_extension.rs의 INSTALLED_AAM_HOME 치환, docs/specs/2026-09-27-release-distribution.md를 읽었다.

[결론] Tauri 표면은 잘 잠겨 있다. CSP는 script-src 'self'(inline 없음), frame/object 차단, connect-src ipc 전용이며 원격 콘텐츠·asset 프로토콜·withGlobalTauri 없음. rpc는 6개 메서드 화이트리스트, 관리 명령은 &'static str 인수만으로 aam을 실행, open_terminal은 POSIX single-quote 이스케이프(shell_quote)로 zsh 스크립트를 0700 파일에 create_new로 쓰고 절대경로 /usr/bin/open을 사용해 셸/AppleScript 주입 경로 없음. 프론트엔드는 React 텍스트 노드만 사용(dangerouslySetInnerHTML·href·window.open 없음), 클립보드에는 shim 실행 명령과 버전 요약만 복사되며 토큰·이메일은 없음. ApiError 메시지에 토큰 패턴 마스킹 있음. aam-observer.js는 O_EXCL|O_NOFOLLOW·0600·소유자/모드 검사·심링크 거부·64KiB 상한을 갖추고 프롬프트·토큰·이메일을 쓰지 않는다(신원은 sha256). INSTALLED_AAM_HOME 치환은 serde_json 문자열로 안전.

[남은 문제] (1) aam-accounts.js가 127.0.0.1:4020에 붙는 상대를 검증하지 않아, 다중 사용자 Mac에서 aam-service가 꺼진 사이 다른 로컬 사용자가 4020을 선점하면 bridge.token과 대화 본문이 그쪽으로 간다(medium). (2) bridge.token이 omp 자격증명 저장소에 10년 만료로 복제되고 회전·폐기 경로가 없다(low). (3) entitlements의 disable-library-validation/allow-unsigned-executable-memory는 Tauri에 불필요한 하드닝 완화(low). (4) `defaults`를 PATH 상대 경로로 실행(low). (5) 공개 전 정리: adhoc 서명·공증/업데이터 부재, 소스 내 제작자 이메일·docs/design HTML의 실제 계정 이메일·/Users 경로(informational). bridge.log에 이메일이 기록되는 점은 CLAUDE.md 규칙과 어긋나지만 기록 주체는 service/bridge.rs라 범위 밖(deferred).

### [medium] omp 확장이 127.0.0.1:4020 상대를 검증하지 않고 브릿지 토큰·대화 본문을 전송
- 위치: `integrations/omp/aam-accounts.js:11`; `integrations/omp/aam-accounts.js:37`; `integrations/omp/aam-accounts.js:52`; `integrations/omp/aam-accounts.js:70`
- 요약: aam-accounts.js는 `http://127.0.0.1:4020`에 붙는 프로세스가 Ojak 브릿지인지 확인하지 않는다. /login 시 `bridge.token`을 Bearer로 보내고, 이후 `ojak-*` 모델의 모든 요청(프롬프트·도구 결과 포함)이 같은 주소로 간다. macOS의 loopback은 사용자 간에 공유되므로, 다중 사용자 Mac에서 aam-service가 내려간 동안(설치 전, `service stop`, 크래시) 다른 로컬 사용자가 4020을 bind하면 토큰과 대화 내용을 수집할 수 있고, 수집한 토큰으로 이후 진짜 브릿지에 접속해 피해자의 구독 계정 한도를 소모할 수 있다. 토큰이 회전하지 않는다는 점(omp-bridge.md 이슈)이 피해를 영구화한다. [INFERENCE] 다중 사용자 환경과 서비스 다운 시점이 전제이며, 단일 사용자 Mac에서는 동일 사용자 프로세스가 이미 bridge.token을 직접 읽을 수 있어 경계가 아니다.
- 권고: 브릿지를 AAM_HOME 안의 Unix domain socket으로 옮기고 omp가 UDS를 지원하면 그것을 사용한다(피어 uid 검증 가능). 불가하면 (a) bridge.token 파일에 설치별 서버 ID를 함께 두고, login 전 인증 없는 `/healthz` 응답의 서버 ID를 대조해 일치할 때만 자격증명을 저장하며, (b) 서비스 쪽은 accept 시 `getpeereid`/`LOCAL_PEERCRED` 대신 TCP라면 `lsof`-free 방식으로 피어 uid를 확인할 수 없으므로 최소한 문서에 다중 사용자 Mac 위험을 명시하고, 토큰 회전·폐기(disconnect 시 재생성) 경로를 추가한다.
- 조치: 미정

### [low] bridge.token이 omp 자격증명 저장소에 10년 만료로 복제되어 회수 불가
- 위치: `integrations/omp/aam-accounts.js:21`; `integrations/omp/aam-accounts.js:50`; `integrations/omp/aam-accounts.js:77`
- 요약: login()/refreshToken()이 `bridge.token` 내용을 access·refresh 양쪽에 넣고 만료를 +10년으로 두어 omp(및 broker 사용 시 auth broker 저장소 [INFERENCE])에 영구 저장한다. `aam omp-bridge disconnect`는 확장만 제거하고 자격증명은 남기며(omp-bridge.md 이슈), 토큰이 회전하지 않으므로 omp 자격증명 파일 백업·동기화·유출 시 브릿지 접근권이 함께 나간다. 토큰이 유효한 동안은 로컬에서 브릿지에 접속해 계정 한도를 소모할 수 있다. 실제 공급자 OAuth 토큰이 같은 저장소에 있어 상대적 위험은 낮다.
- 권고: expires를 짧게(예: 1시간) 두고 refreshToken()이 매번 bridge.token을 다시 읽게 하면 파일 재생성만으로 폐기가 된다. disconnect 시 bridge.token을 재생성하고 broker/omp의 ojak-* 자격증명을 삭제한다.
- 조치: 미정

### [low] entitlements가 Tauri에 불필요한 하드닝 런타임 완화를 요청
- 위치: `apps/desktop/src-tauri/entitlements.plist:5`; `apps/desktop/src-tauri/tauri.conf.json:27`
- 요약: entitlements.plist가 `disable-library-validation`과 `allow-unsigned-executable-memory`를 켠다. WKWebView는 JS를 별도 WebContent 프로세스에서 실행하므로 앱 프로세스에는 이 둘이 필요 없다. 라이브러리 검증을 끄면 번들 안 Frameworks/Resources에 놓인 서명 불일치·무서명 dylib이 로드되어, 나중에 Developer ID로 서명·공증하더라도 ~/Applications의 앱 번들을 수정할 수 있는 프로세스가 코드 주입 경로를 얻는다. 현재 adhoc 서명 상태에서는 번들 자체가 재서명 가능해 추가 위험은 작지만, 공증 전환 시 Apple이 이 항목의 사유를 요구한다.
- 권고: `disable-library-validation`·`allow-unsigned-executable-memory`를 제거하고 빌드·실행을 확인한다. Tauri 2 macOS 앱은 일반적으로 엔타이틀먼트 없이도 동작하며, 필요하면 `allow-jit`만 남긴다.
- 조치: 수정(2026-09-28): disable-library-validation·allow-unsigned-executable-memory 제거. allow-jit만 유지

### [low] `defaults`를 PATH 상대 경로로 실행
- 위치: `apps/desktop/src-tauri/src/main.rs:824`
- 요약: 시스템 언어 감지가 `Command::new("defaults")`로 PATH를 검색한다. 같은 파일의 다른 외부 명령은 `/usr/bin/open` 절대 경로를 쓴다. 앱이 터미널·Orca·omp 등 PATH가 바뀐 환경에서 실행되면 앞선 PATH 항목의 `defaults`가 앱 프로세스 권한으로 시작 직후 실행된다. 동일 사용자 경계라 영향은 제한적이지만 코드 서명·공증 후에도 앱 프로세스로 임의 코드를 넣는 손쉬운 경로다.
- 권고: `Command::new("/usr/bin/defaults")`로 고정하거나 `NSLocale.preferredLanguages`를 objc로 읽는다.
- 조치: 수정(2026-09-28): `/usr/bin/defaults` 절대 경로로 실행

### [informational] 공개 전 정리: 제작자 이메일·실제 계정 이메일·절대 경로가 소스와 문서에 남음, adhoc 서명·업데이터 부재
- 위치: `apps/desktop/src-tauri/src/main.rs:678`; `apps/desktop/src/App.tsx:85`; `docs/spec.md:21`; `docs/spec.md:631`
- 요약: main.rs의 `contact_author`가 개인 Gmail 주소를 하드코딩하고, docs/spec.md에 개발 Mac 절대 경로(`<repo>`)와 실제 계정 이메일(alice, bob, carol)이 기록되어 있으며, docs/design/*.html 목업 데이터에도 같은 실제 이메일이 있다. 첫 커밋에 포함되면 git 이력에서 제거하기 어렵다. 또한 tauri.conf.json은 adhoc 서명(`signingIdentity: "-"`)이며 공증·자동 업데이트가 없어 사용자는 Gatekeeper를 우회(`xattr -d`)해 실행해야 하고 보안 수정이 배포되지 않는다. release-distribution.md에 이미 계획으로 적혀 있어 확인 차원의 기록이다.
- 권고: 연락처를 빌드 시 환경변수/설정으로 주입하거나 GitHub Issues 링크로 대체. docs/spec.md·docs/design 목업의 이메일을 example.com으로 치환하고 절대 경로를 제거한 뒤 첫 커밋. 배포 전 Developer ID 서명·공증과 Tauri updater(서명 키) 도입.
- 조치: 연락 주소는 소스에 두지 않고 빌드 환경변수 `OJAK_CONTACT_URL`로만 넣는다. 문서의 개발 머신 경로·내부 호스트·OS 사용자명은 자리표시자로 바꿨다. 목업 이메일은 `example.com`이다. adhoc 서명·공증은 배포 정책의 남은 항목이다.

## loopback 상대 검증 조사 (2026-09-27)

인증되지 않은 `/healthz`에 고정 식별자를 넣어 대조하는 방식은 채택하지 않는다. 다른 사용자가 정상 서비스에서 값을 읽어 가짜 서버에서 재생할 수 있고, 로그인 때만 확인해서는 이후 요청의 포트 선점을 막지 못한다. 연결마다 인증된 전송(피어 uid를 확인하는 Unix socket 또는 고정 인증서 TLS)이 필요하다.

| 구간 | Unix socket 가능 여부 | 근거·방법 |
|---|---|---|
| omp 확장 → 브릿지(:4020) | 가능하지만 침습적 | omp에는 소켓 설정이 없다. pi-native 클라이언트가 요청마다 `globalThis.fetch`를 쓰고 확장은 같은 프로세스에서 돌므로, 확장이 `globalThis.fetch`를 감싸 Bun `unix:` 옵션을 붙일 수 있다(`pi-ai/src/providers/pi-native-client.ts`, `utils/transport-fetch.ts`). 다만 omp 안의 모든 요청 경로를 가로채는 방식이라 결정 필요. |
| 브릿지 → gateway(:41xx) | 불가(omp 미지원) | `auth-gateway serve`는 `--bind=host:port`만 받는다(`pi-ai/src/auth-gateway/server.ts`, `utils/parse-bind.ts`). 대안: `--bind=127.0.0.1:0`로 임의 포트를 받고, 연결 소켓을 자식 pid의 fd와 대조(`proc_pidfdinfo`). |
| 브릿지 → broker(:8765) | 불가(omp 미지원) | 서버·클라이언트 모두 TCP만. 사용자 omp 설정도 같은 주소를 쓴다. omp 상위 수정이 필요하다. |

- 조치: 미정 (사용자 결정 대기). 단일 사용자 Mac에서는 경계가 아니다.
