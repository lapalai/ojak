---
generated: 2026-09-27
last_sync: 2026-09-27
last_full_scan: 2026-09-27
sync_count: 0
last_screen_flow_sync: 2026-09-27
generator: init-docs-v3
project: ojak
framework_type: monorepo-mixed
domain_count: 7
sub_projects:
  - {name: service, path: crates/service, type: web-backend}
  - {name: launcher, path: crates/launcher, type: cli-tool}
  - {name: adapters, path: crates/adapters, type: library}
  - {name: protocol, path: crates/protocol, type: library}
  - {name: desktop, path: apps/desktop, type: web-frontend}
---

# Domain Index

| 도메인 | 경로 | 요약 |
|---|---|---|
| [protocol](protocol.md) | `crates/protocol/**` | 공유 타입·상수·Unix socket RPC 프레이밍 |
| [service-core](service-core.md) | `crates/service/src/{lib,server,store,scheduler,routes,managed_sessions,diagnostics,process}.rs` | aam-service 데몬: RPC, SQLite, lease 상태기계, 배정 |
| [omp-bridge](omp-bridge.md) | `crates/service/src/bridge.rs`, `crates/launcher/src/omp_{bridge,broker}.rs`, `integrations/omp/aam-accounts.js` | omp `ojak-*` 공급자 → :4020 브릿지 → 계정별 gateway |
| [adapters](adapters.md) | `crates/adapters/**` | 공식 CLI 탐지·identity·quota·안전 검사 |
| [launcher](launcher.md) | `crates/launcher/src/{main,lib,arguments,supervisor,descendants,install,shell,hosts}.rs` | `aam` CLI + claude/codex shim + 설치 |
| [omp-observer](omp-observer.md) | `integrations/omp/aam-observer*`, `crates/launcher/src/omp_{observer,extension}.rs` | 읽기 전용 omp 세션 관측 |
| [desktop](desktop.md) | `apps/desktop/**`, `scripts/*.mjs` | Tauri + React 대시보드 |
