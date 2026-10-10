# omp-observer

## 개요
omp 확장이 세션마다 스냅샷을 `<AAM_HOME>/omp-observations/<pid>-<sha256(sessionId)>.json`에 쓴다. 서비스가 adapters로 읽어 스냅샷의 외부 세션·호출 기록에 붙인다. 요청 경로만 담은 `aam-route` custom entry도 omp 세션에 저장한다(LLM 문맥에는 포함되지 않음). 계정을 고르거나 omp 인증을 바꾸지 않는다.

## 구성
- 확장: `integrations/omp/aam-observer.js` — 기본 export `aamObserver`(`:147`), `writeSnapshot`(`:49`), 구독 이벤트(`:268`)
- 테스트: `integrations/omp/aam-observer.test.mjs` (`node --test`)
- 설치: `crates/launcher/src/omp_observer.rs` (`aam omp-observer status|install|uninstall`)
- 공통 설치기: `crates/launcher/src/omp_extension.rs` — `.aam-owner.json` 소유 기록, 설치 경로와 helper의 canonical 경로를 치환한 뒤 해시한다. 앱과 symlink shim으로 실행해도 동일한 설치본을 최신으로 판정한다.
- 읽기: `crates/adapters/src/observed.rs`(`discover_sessions`), `observed_metadata.rs`; Windows 프로세스·쓰기 핸들 근거는 `observed_windows.rs`.

## 주의
- `AAM_HOME` 우선순위가 두 확장에서 다르다: `aam-observer.js`는 env 먼저, `aam-accounts.js`는 설치 시 치환값 먼저.
- `AAM_HOME`이 바뀌면 설치된 확장 해시가 달라져 "최신 아님"으로 보인다. 다시 설치하면 된다.
- 수정된 기존 파일·symlink 충돌은 덮어쓰지 않는다.
- Windows는 JS의 Unix uid 검사 대신 `omp_observer_writer.rs`의 native writer가 현재 사용자 ACL·단일 링크 일반 파일·재분석 지점 여부와 부모 프로세스 identity를 검사하고 원자적으로 스냅샷을 쓴다. 실패하면 성공 스냅샷으로 취급하지 않는다.
- 새 관측 확장은 설치된 자신의 진입 파일을 감시한다. 확장이 제거·교체되면 timer와 추가 관측을 멈추고 진행 중인 쓰기를 마친 뒤 writer의 실제 `close`까지 기다린다. `session_shutdown`도 같은 방식으로 실행 파일 핸들을 해제한다. 이미 실행 중인 구버전 확장은 omp를 다시 열거나 reload해야 이 동작이 적용된다.
- 외부 세션 파일은 실제 OMP 프로세스의 열린 쓰기 핸들로 확인한다. 파일명·mtime·observer의 경로 선언만으로 열린 세션을 추정하지 않는다. 아직 저장 파일이 없는 새 대화는 프로세스만 관측될 수 있다.

## 요청 경로 관측
- macOS 실요청 검증 버전: **18.4.5**(기존 경로 hook 검증: 18.4.4). `before_provider_request`와 `after_provider_response`의 요청별 `ctx.model`을 사용한다. Codex WebSocket은 응답 hook이 없고, pi-native는 payload hook이 없으므로 둘 다 필요하다.
- Ojak 공급자 + `pi-native` + `http://127.0.0.1:4020`이면 `bridge`; 지원 원본 공급자의 다른 HTTP(S) endpoint와 비 pi-native 전송이면 `direct`; 나머지는 `unknown`. URL·본문·응답 헤더·토큰은 기록하지 않는다.
- `aam-route` custom data: `{ version: 1, provider, model, route, recordedAt }`. observer의 호출 묶음 키에 route를 포함해 같은 모델의 경유·직접 요청을 모두 유지한다. 요청 준비 관측은 응답 완료·계정 identity·역할명·호출 횟수의 증거가 아니다.
- Rust는 `customType === "aam-route"`만 별도 파싱한다. 서비스 재시작이나 확장 스냅샷 만료 후에도 세션 파일의 경로 근거를 복원한다. 기존 읽기·행 수 제한은 유지한다.
- 완료 메시지의 provider는 gateway의 upstream 이름일 수 있다. `message_end`·`model_usage`·과거 기록에 경로가 없으면 미경유로 추정하지 않는다. 경로 기록과 완료 기록을 시각이나 폴더로 억지 연결하지 않는다.
- 연결 내역·경로 확인 불가 경고는 명시적인 `route: direct|unknown` 관측만 표시한다. `route: bridge`는 브릿지 사용량에서 다루고, 경로가 없는 완료·보조 호출·모델 선택 기록은 연결 상태 판정에서 제외한다. 따라서 Ojak 경유 응답에 upstream 공급자 이름이 남아도 별도의 “확인 필요” 행을 만들지 않는다. 구형 기록의 경로는 여전히 알 수 없으며, 경고에서 제외한다고 경유로 확정하는 것은 아니다.
- 확장 갱신: 앱을 열면 현재 설치본이 소유한 기존 확장을 자동으로 맞춘다(`aam integration refresh-extensions`). 수동으로는 새 `aam`으로 `aam omp-observer uninstall` → `aam omp-observer install`. 실행 중인 omp는 강제 종료하지 않으며, 새 omp 실행 또는 확장 reload 후 새 요청부터 새 관측 코드가 동작한다. 과거 경로는 소급 생성하지 않는다.


## Screen Flow / Lifecycle
<!-- screen-flows-v3: 2026-09-27, type=library -->

| Stage | 상태 | 근거 |
|---|---|---|
| Module | ✅ | JS: integrations/omp/aam-observer.js:147 `export default function aamObserver(pi)` (single public export, embedded via crates/launcher/src/omp_observer.rs:5 inc |
| Function | ⚡ | Surface is implemented, but the docs and the desktop exposure are incomplete. JSDoc only on aamObserver (aam-observer.js:146). The Rust pub items `discover_sess |
| Example | ⚡ | No README or examples/ directory. Usage appears only in tests and docs: 5 node tests (integrations/omp/aam-observer.test.mjs:75,101,115,131,141, run via CLAUDE. |

```
omp-observer
├── integrations/omp/aam-observer.js  (OMP ExtensionAPI v18.2.6)
│   ├── export default aamObserver(pi: ExtensionAPI): void        :147
│   │   ├── subscribed OMP events
│   │   │   ├── session_start   → observe(ctx, isIdle? idle:running)   :268
│   │   │   ├── session_switch  → 〃                                   :268
│   │   │   ├── session_branch  → 〃                                   :268
│   │   │   ├── session_tree    → 〃                                   :268
│   │   │   ├── session_compact → 〃                                   :268
│   │   │   ├── agent_start     → observe(ctx,'running')               :274
│   │   │   ├── turn_end        → observe(ctx)                         :275
│   │   │   ├── agent_end       → observe(ctx, willContinue?running:idle) :276
│   │   │   ├── message_end     → rememberCall(assistant provider/model/stopReason) :277
│   │   │   └── session_shutdown→ lifecycle='shutdown', clearTimer, flush(true) :292
│   │   ├── ctx.setInterval(flush, 1s) / heartbeat 15s             :10-11
│   │   └── snapshot() → {version:1,pid,sessionId,sessionFile,observedAt,startedAt,lifecycle,pins,selections,calls,issues,parentSessionFile?,leafId?} :232
│   ├── writeSnapshot(root, snapshot): Promise<void>  (0700 dir, 0600 O_EXCL tmp → rename, 64KiB) :49
│   ├── currentPins(manager, issues): Pin[]  (branch ≤4096)       :83
│   ├── auxiliaryCalls(manager, calls, issues)  (model_usage ≤2048) :124
│   ├── rememberCall(calls, call)  (≤64)                          :115
│   └── identityHash(provider, identity): sha256 | undefined      :34
├── crates/launcher  (`aam omp-observer status|install|uninstall`)
│   ├── omp_observer::run(action: &str) -> Result<String /*JSON*/>  omp_observer.rs:10
│   └── omp_extension (pub(crate), shared with omp_bridge)
│       ├── struct Extension { source, owner, directory }          :15
│       ├── extension_directory(&Extension) -> PathBuf             :54
│       ├── inspect(&Extension, &Path) -> Option<Installed{current}> :143
│       ├── install(&Extension, &Path) -> bool                     :195
│       └── uninstall(&Extension, &Path) -> bool                   :234
├── crates/adapters  (reader)
│   ├── pub fn discover_sessions(&Paths, &[Account]) -> ObservedScan  observed.rs:138
│   ├── pub struct ObservedScan { sessions, notices }             observed.rs:12
│   └── observed_metadata (pub(super))
│       ├── read_sessions(&ProcessIdentity, &[WriterFile], &mut budget) :409
│       ├── attribute(paths, identity, writer, session, role, accounts) -> (Vec<ObservedAttribution>, Option<&str>) :554
│       ├── bridge_valid(bridge, identity, writer, session, mtime, now) -> bool  (lifecycle ∈ running|idle, fresh ≤90s) :500
│       └── struct Bridge (v1 snapshot schema)                    :98
└── consumers
    ├── service Snapshot::snapshot() → discover_sessions          crates/service/src/lib.rs:261
    ├── desktop UsageView session.attributions                    apps/desktop/src/UsageView.tsx:97
    └── tauri omp_observer_action (registered, no UI caller)      apps/desktop/src-tauri/src/main.rs:427
```

```mermaid
graph LR
  OMP[OMP process events] --> EXT[aam-observer.js]
  EXT -->|writeSnapshot| FS[AAM_HOME/omp-observations/pid-sha.json]
  CLI[aam omp-observer install] -->|omp_extension::install| INST[~/.omp/agent/extensions/aam-observer/index.js]
  INST -.loaded by.-> OMP
  FS -->|attribute + bridge_valid| AD[adapters discover_sessions]
  AD --> SVC[service snapshot]
  SVC --> UI[desktop UsageView]
```

### 이슈
- [medium] Function: Stale snapshots are never removed. The JS writes `<pid>-<sha256(sessionId)>.json` for every process and session (aam-observer.js:49-81), and nothing in integrations/ or crates/ deletes them: `omp-observations` appears on
- [low] Function: Tauri command `omp_observer_action` (apps/desktop/src-tauri/src/main.rs:427, :998) has no frontend invoker under apps/desktop/src. docs/spec.md:155 says install is CLI-only, while docs/spec.md:572 says it happens on the 
- [low] Function: Schema limits have almost no headroom. Rust accepts at most 16 issues (observed_metadata.rs:531), and the JS can already emit 15 distinct codes (4 BASE_ISSUES at aam-observer.js:19 plus 11 conditional ones). One new issu
- [low] Function: Upgrading is inconsistent. `aam omp-observer install` on an outdated copy returns an error asking for uninstall then install (omp_extension.rs:199-201). The omp_bridge flow instead auto-reinstalls (omp_bridge.rs:250-251)
- [low] Function: Public Rust items have no doc comments: `ObservedScan` (observed.rs:12), `omp_observer::run` (omp_observer.rs:10), and omp_extension `inspect`/`install`/`uninstall` (:143,:195,:234). The v1 snapshot schema is documented 

### 다음 할 일
- [ ] Function: skip the useless 'shutdown' write, or unlink the owned snapshot on session_shutdown (aam-observer.js:292-302). Add best-effort pruning of snapshots whose pid has exited, or that are older than FRESH_MS, to the adapters read path or the service.
- [ ] Function: either wire `omp_observer_action` into the connection screen or remove the Tauri command, then reconcile docs/spec.md:155 with :572.
- [ ] Function: raise the Rust issue cap (observed_metadata.rs:531) or define the issue enum in one shared place, so the JS and Rust limits cannot drift.
- [ ] Function: add doc comments to ObservedScan, omp_observer::run and omp_extension inspect/install/uninstall. Document the v1 snapshot schema in docs/domains/omp-observer.md, with field table and limits (calls ≤64, pins/providers ≤32, 64KiB/128KiB, 90s freshness).
- [ ] Function: consider aligning `aam omp-observer install` with omp_bridge's automatic uninstall and reinstall when the installed copy is outdated.
