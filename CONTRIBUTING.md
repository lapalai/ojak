# 기여 안내 / Contributing

## 지원 범위 / Scope

- macOS 13 이상, Apple Silicon(arm64)이 주 대상입니다. Windows 10 1809+ / Windows 11 x64 미리보기도 지원합니다. / macOS 13+, Apple Silicon is the primary target. A Windows 10 1809+ / Windows 11 x64 preview is also supported.
- Claude Code, Codex의 공식 CLI와 omp. / Official Claude Code and Codex CLIs, and omp.

## 받지 않는 변경 / Out of scope

- 인증을 우회하는 경로. API 키·base URL·auth helper로 자동 전환하지 않습니다. / Anything that bypasses authentication or silently switches to API keys, base URLs or auth helpers.
- 한도 회피, 계정 공유를 목적으로 한 기능. / Features meant to evade limits or share accounts.
- 토큰·이메일·프롬프트를 로그·오류·진단에 남기는 변경. / Changes that put tokens, emails or prompts into logs, errors or diagnostics.
- 불확실할 때 lease(동시 실행 자리)를 시간 초과만으로 푸는 변경. / Releasing a lease on timeout alone when its state is uncertain.

## 개발 / Development

Rust stable과 Node.js 22가 필요합니다. Mac은 Apple Silicon, Windows는 x64입니다. Windows 11에는 WebView2가 들어 있고, Windows 10은 따로 설치해야 합니다. 설치기를 관리자 권한으로 빌드하거나 실행하지 마세요.

Rust stable and Node.js 22 are required. Mac builds are Apple Silicon. Windows builds are x64. WebView2 is included on Windows 11 and must be installed separately on Windows 10. Do not build or run the installer as administrator.

```sh
npm ci
~/.cargo/bin/cargo test            # Mac. PATH에 없으면 이 경로. default-members만, Tauri crate 제외
cargo test                         # Windows. 같은 default-members / same default-members
npm run typecheck
npm run test:desktop
node --test integrations/omp/aam-observer.test.mjs integrations/omp/aam-accounts.test.mjs
npm run build
```

`cargo`가 PATH에 있으면 Mac에서도 `cargo test`로 충분합니다. CI는 macOS 14와 windows-latest에서 `cargo test --workspace --exclude ai-account-manager -- --test-threads=1`을 돌립니다. 로컬 default-members와 같은 크레이트 집합입니다.

If `cargo` is on PATH, `cargo test` is enough on Mac too. CI runs `cargo test --workspace --exclude ai-account-manager -- --test-threads=1` on macOS 14 and windows-latest. That is the same crate set as local default-members.

결과: Mac은 `target/release/bundle/macos/Ojak.app`, Windows는 `target/release/bundle/nsis/*-setup.exe`. / Output is `target/release/bundle/macos/Ojak.app` on Mac and `target/release/bundle/nsis/*-setup.exe` on Windows.

구조와 규칙은 `CLAUDE.md`, `docs/domains/INDEX.md`를 먼저 읽어 주세요. / Read `CLAUDE.md` and `docs/domains/INDEX.md` first.

## PR

- 동작이 바뀌면 테스트와 해당 `docs/domains/*.md`를 함께 고칩니다. / Update tests and the matching `docs/domains/*.md` with behavior changes.
- UI 문구는 `apps/desktop/src/i18n.ts`의 ko·en·id를 모두 채웁니다. / Fill ko, en and id in `apps/desktop/src/i18n.ts`.
- 토큰·이메일·프롬프트·프로필 경로를 diff에 넣지 마세요. / Do not put tokens, emails, prompts, or profile paths in the diff.
- 이 PR을 여는 것으로 기여를 MIT로 제공하는 데 동의합니다. Signed-off-by나 DCO는 받지 않습니다. / By opening a pull request you agree the contribution is licensed under MIT. No sign-off or DCO is required.
- 보안 문제는 이슈 대신 [SECURITY.md](SECURITY.md)의 비공개 보고로 알려 주세요. / Report security issues privately, as described in [SECURITY.md](SECURITY.md).
