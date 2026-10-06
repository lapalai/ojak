# 보안 정책 / Security policy

Ojak은 미리보기(0.1.x)입니다. 장기 지원 브랜치는 없습니다. 보안 수정은 `main`의 최신 커밋과, 공개 릴리스가 생긴 뒤에는 **가장 최근 GitHub 릴리스**에만 들어갑니다. 그 이전 태그는 고치지 않습니다.

This project is a pre-release (0.1.x). There is no long-term support branch. Fixes land on the latest `main` commit and, once a public release exists, only on the latest GitHub release.

## 지원 버전 / Supported versions

| 버전 / Version | 지원 / Supported |
|---|---|
| 최신 `main`, 그리고 공개 후 최신 릴리스 / latest `main`, and the latest release after one is published | 예 / yes |
| 그 이전 릴리스 / older releases | 아니오 / no |

Windows 빌드는 x64 미리보기입니다. 같은 지원 정책이 적용됩니다. / The Windows build is an x64 preview under the same policy.

## 비공개 보고 / Private reporting

공개 이슈, PR, 토론에 올리지 마세요. 토큰·세션 쿠키·프로필 파일을 붙이지 마세요.

Do not open a public issue, pull request, or discussion. Do not attach tokens, session cookies, or profile files.

보고 주소 / Report here:

https://github.com/lapalai/ojak/security/advisories/new

**접수 확인 목표: 영업일 7일.** 심각도 판단과 다음 연락 시점은 그 답변에 적습니다. 버그 바운티는 없습니다. 이 기한은 목표이지 보장된 SLA가 아닙니다.

**Acknowledgement target: 7 business days.** The reply will include a severity judgement and when to expect the next update. There is no bug bounty. The window is a target, not a guaranteed SLA.

저장소가 비공개이거나 Private vulnerability reporting이 꺼져 있으면 위 주소가 열리지 않습니다. 그때는 공개 이슈로 세부 내용을 올리지 말고, 저장소 관리자에게 연락 경로만 물어 보세요.

If that URL does not open, private vulnerability reporting is not enabled yet. Ask a maintainer for a contact path. Do not put details in a public issue.

## 범위 / In scope

- 인증 우회. API 키·base URL·auth helper로 구독 로그인을 바꾸거나, 그 검사를 건너뛰는 경로. / Authentication bypass, including skipping the API key, base URL, or auth-helper checks.
- 토큰·이메일·프롬프트·프로필 경로가 로그, 오류, 진단 내보내기, 업데이터에 새는 경우. / Token, email, prompt, or profile-path leakage into logs, errors, diagnostics export, or the updater.
- 설치기·서비스의 권한 상승. 사용자 폴더 밖으로 쓰거나, 관리자 권한 없이 다른 사용자 데이터를 읽는 경우. / Privilege escalation in the installer or service, including writes outside the user profile or cross-user reads.
- shim 탈취. PATH, 재귀 shim, 진입 경로 바꿔치기로 다른 바이너리를 실행하게 하는 경우. / Shim hijack via PATH, recursive shims, or swapping the CLI entry path.
- 로컬 `bridge.token` 유출, 또는 서명 검증을 우회하는 업데이트. / Leak of the local `bridge.token`, or an update that bypasses signature checks.

## 범위 밖 / Out of scope

- 본인 계정으로 정상 사용하는 것, 공급자 약관·계정 정지에 대한 문의. / Normal use of accounts you own, and provider terms or account bans.
- 한도 회피, 계정 공유를 요청하는 것. / Requests to evade limits or share accounts.
- Claude Code, Codex, omp, 운영체제 자체의 취약점. Ojak 연동이 그것을 악용 가능하게 만들지 않는 한 해당 프로젝트에 보고하세요. / Vulnerabilities in Claude Code, Codex, omp, or the OS, unless Ojak's integration makes them exploitable.
- 이미 로그인한 같은 사용자가 자기 데이터 폴더를 읽는 것. / The logged-in user reading their own data directory.
- README에 이미 적힌 미서명·미공증 설치 경고. / The unsigned and unnotarized install warnings already documented in the README.
- 서비스 거부, 소셜 엔지니어링, 물리 접근. / Denial of service, social engineering, and physical access.

## 자격 증명과 진단 / Credentials and diagnostics

Ojak의 SQLite에는 공급자 API 키나 OAuth 액세스·리프레시 토큰을 저장하지 않습니다. 계정 행은 표시 이름, 이메일, 플랜, 프로필 경로, 바이너리 경로, identity, 사용량만 둡니다. omp 핀은 자격 증문이 아니라 해시입니다.

Ojak's SQLite does not store provider API keys or OAuth access or refresh tokens. Account rows keep a label, email, plan, profile path, binary path, identity, and usage. omp pins are hashes, not credentials.

로그인 파일 자체는 공식 CLI가 계정별 프로필 폴더에 씁니다. Claude는 `.credentials.json`, Codex는 `auth.json`입니다. 그 폴더는 이 컴퓨터의 Ojak 데이터 디렉터리 안에 있습니다. Ojak은 그 토큰을 데이터베이스, 로그, 진단 파일로 복사하지 않습니다.

The official CLIs write login files into the per-account profile directory (Claude `.credentials.json`, Codex `auth.json`). That directory lives inside Ojak's data directory on this computer. Ojak does not copy those tokens into its database, logs, or diagnostics file.

진단 내보내기(`diagnostics.export`)는 원문을 직렬화한 뒤 지우는 방식이 아닙니다. 허용된 필드만 새로 만들고, `redacted: true`로 표시합니다. 이메일, 경로, 토큰, 프롬프트, 세션 식별자, capability는 넣지 않으며 계정은 `account-N` 별칭으로 바꿉니다. `redact: false`는 거부합니다. 앱의 저장 대화상자도 개인정보를 제외한다고 표시합니다. Ojak은 이 파일을 업로드하지 않습니다.

Diagnostics export does not serialize the raw record and then delete fields. It builds an allowlist and marks the file `redacted: true`. Emails, paths, tokens, prompts, session identifiers, and capabilities are omitted. Accounts become `account-N` aliases. `redact: false` is rejected. The save dialog says private data is excluded. Ojak does not upload the file.

omp 브릿지를 켜면 로컬 루프백 인증용 `bridge.token`을 두고, 구독 자격 증명은 그 컴퓨터의 프록시로만 통과합니다. 브릿지는 기본으로 꺼져 있습니다.

If the omp bridge is enabled, Ojak keeps a local `bridge.token` for loopback authentication and passes subscription credentials only through a proxy on that computer. The bridge is off by default.
