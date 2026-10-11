// AI Account Manager 소유 확장. OMP 공개 ExtensionAPI의 요청별 model을 관측합니다.
import { createHash, randomBytes } from 'node:crypto';
import { spawn } from 'node:child_process';
import { constants, watch } from 'node:fs';
import { lstat, mkdir, open, rename, unlink } from 'node:fs/promises';
import { homedir } from 'node:os';
import { isAbsolute, join, resolve } from 'node:path';

// native installer가 manager 경로와 ACL-aware writer의 진입 경로를 치환합니다.
const INSTALLED_AAM_HOME = undefined;
const INSTALLED_AAM_WRITER = undefined;
const INSTALLED_AAM_ENTRY = undefined;
const WRITE_INTERVAL = 1_000;
const HEARTBEAT_INTERVAL = 15_000;
const BRANCH_LIMIT = 4_096;
const USAGE_LIMIT = 2_048;
const CALL_LIMIT = 64;
const PROVIDER_LIMIT = 32;
const SNAPSHOT_LIMIT = 65_536;
const HASH_PATTERN = /^[a-f0-9]{64}$/;
const STOP_REASONS = new Set(['stop', 'length', 'toolUse', 'error', 'aborted']);
const BASE_ISSUES = ['request-account-unavailable', 'pin-history-only', 'auxiliary-history-only', 'provider-session-identity-unavailable'];
const ROUTE_ENTRY = 'aam-route';
const BRIDGE_PROVIDERS = new Set(['ojak-claude', 'ojak-codex', 'ojak-antigravity', 'ojak-grok', 'ojak-zai', 'aam-claude', 'aam-codex', 'aam-antigravity', 'aam-grok', 'aam-zai']);
const DIRECT_PROVIDERS = new Set(['anthropic', 'openai-codex', 'google-antigravity', 'xai-oauth', 'zai']);

// message_end의 provider는 gateway의 upstream 이름일 수 있다. 반드시 요청/응답 hook의
// 실제 요청 model만 사용하며, URL·헤더·토큰은 저장하지 않는다.
function requestRoute(model) {
  try {
    const url = new URL(model.baseUrl);
    if (!['http:', 'https:'].includes(url.protocol) || url.username || url.password) return 'unknown';
    const ojak = url.origin === 'http://127.0.0.1:4020';
    if (ojak && model.transport === 'pi-native' && BRIDGE_PROVIDERS.has(model.provider)) return 'bridge';
    if (!ojak && model.transport !== 'pi-native' && DIRECT_PROVIDERS.has(model.provider)) return 'direct';
  } catch {}
  return 'unknown';
}

function routeCall(data) {
  if (!data || data.version !== 1 || !['bridge', 'direct', 'unknown'].includes(data.route)) return undefined;
  const provider = text(data.provider);
  const model = text(data.model);
  const recordedAt = time(data.recordedAt);
  if (!provider || !model || recordedAt === undefined) return undefined;
  return { provider, model, purpose: 'provider-route', route: data.route, recordedAt };
}

function text(value, limit = 256) {
  return typeof value === 'string' && value.length > 0 && Buffer.byteLength(value) <= limit && !/[\u0000-\u001f\u007f]/u.test(value)
    ? value : undefined;
}

function time(value) {
  return Number.isSafeInteger(value) && value >= 0 ? value : undefined;
}

function digest(value) {
  return createHash('sha256').update(value).digest('hex');
}

function identityHash(provider, identity) {
  // 대소문자/공백을 정규화하면 OMP credential_pin 해시와 달라집니다.
  const fields = [identity.accountId, identity.email, identity.orgId, identity.projectId];
  if (fields.some(value => value !== undefined && (typeof value !== 'string' || value.length > 4_096))) return undefined;
  if (!fields[0] && !fields[1]) return undefined;
  return digest([provider, ...fields.map(value => value ?? '')].join('\0'));
}

// Windows does not implement Unix uid/mode ownership checks. A persistent native
// child performs the ACL/reparse-point checks and atomic writes instead.
function nativeWriter(root) {
  let child;
  let pending;
  let closing;
  function fail() {
    const current = child;
    child = undefined;
    current?.kill();
    if (pending) {
      clearTimeout(pending.timer);
      const { reject } = pending;
      pending = undefined;
      reject(new Error('native-writer-unavailable'));
    }
  }
  return {
    write(data) {
      if (closing) return Promise.reject(new Error('native-writer-closed'));
      if (pending) return Promise.reject(new Error('native-writer-busy'));
      if (!child) {
        const executable = INSTALLED_AAM_WRITER || join(process.env.LOCALAPPDATA || join(homedir(), 'AppData/Local'), 'Ojak/aam.exe');
        const current = spawn(executable, ['omp-observer', 'write'], {
          env: { ...process.env, AAM_HOME: root }, windowsHide: true,
          stdio: ['pipe', 'pipe', 'ignore'],
        });
        child = current;
        current.unref();
        let response = '';
        current.stdout.on('data', chunk => {
          if (current !== child) return;
          response += chunk.toString('utf8');
          if (response.length > 16) { fail(); return; }
          if (!response.endsWith('\n')) return;
          const result = response;
          response = '';
          if (!pending) { fail(); return; }
          const request = pending;
          pending = undefined;
          clearTimeout(request.timer);
          current.stdin.unref?.();
          current.stdout.unref?.();
          if (result === 'ok\n') request.resolve();
          else request.reject(new Error('unsafe-snapshot'));
        });
        current.on('error', () => { if (child === current) fail(); });
        current.on('exit', () => { if (child === current) fail(); });
        current.stdin.on('error', () => { if (child === current) fail(); });
      }
      return new Promise((resolve, reject) => {
        pending = { resolve, reject, timer: setTimeout(fail, 10_000) };
        child.stdin.ref?.();
        child.stdout.ref?.();
        child.stdin.write(`${data}\n`);
      });
    },
    close() {
      if (!child) return closing;
      const current = child;
      current.ref();
      closing = new Promise(resolve => current.once('close', resolve));
      if (pending) fail();
      else { current.stdin.end(); child = undefined; }
      return closing;
    },
  };
}

async function ownedDirectory(path) {
  await mkdir(path, { recursive: true, mode: 0o700 });
  const stat = await lstat(path);
  if (!stat.isDirectory() || stat.isSymbolicLink() || stat.uid !== process.getuid?.() || (stat.mode & 0o077) !== 0) {
    throw new Error('unsafe-directory');
  }
}
async function writeSnapshot(root, snapshot, writer) {
  let data = JSON.stringify(snapshot);
  while (Buffer.byteLength(data) > SNAPSHOT_LIMIT && snapshot.calls.length > 0) {
    snapshot.calls.shift();
    if (!snapshot.issues.includes('snapshot-call-limit')) snapshot.issues.push('snapshot-call-limit');
    data = JSON.stringify(snapshot);
  }
  if (Buffer.byteLength(data) > SNAPSHOT_LIMIT) throw new Error('snapshot-limit');
  if (writer) { await writer.write(data); return; }
  await ownedDirectory(root);
  const directory = join(root, 'omp-observations');
  await ownedDirectory(directory);
  const destination = join(directory, `${snapshot.pid}-${digest(snapshot.sessionId)}.json`);
  try {
    const stat = await lstat(destination);
    if (!stat.isFile() || stat.isSymbolicLink() || stat.uid !== process.getuid?.() || (stat.mode & 0o077) !== 0) {
      throw new Error('unsafe-file');
    }
  } catch (error) {
    if (error?.code !== 'ENOENT') throw error;
  }
  const temporary = join(directory, `.${snapshot.pid}-${randomBytes(12).toString('hex')}.tmp`);
  let handle;
  try {
    handle = await open(temporary, constants.O_WRONLY | constants.O_CREAT | constants.O_EXCL | constants.O_NOFOLLOW, 0o600);
    await handle.writeFile(data, 'utf8');
    await handle.close();
    handle = undefined;
    await rename(temporary, destination);
  } finally {
    await handle?.close().catch(() => {});
    await unlink(temporary).catch(() => {});
  }
}

function currentPins(manager, issues) {
  const pins = new Map();
  const latestAssistant = new Map();
  const visited = new Set();
  let id = manager.getLeafId();
  while (id !== null && id !== undefined && visited.size < BRANCH_LIMIT) {
    if (visited.has(id)) { issues.add('branch-incomplete'); break; }
    visited.add(id);
    const entry = manager.getEntry(id);
    if (!entry) { issues.add('branch-incomplete'); break; }
    if (entry.type === 'credential_pin') {
      const provider = text(entry.provider);
      if (provider && !pins.has(provider) && HASH_PATTERN.test(entry.hash ?? '')) {
        const at = time(Date.parse(entry.timestamp));
        if (at !== undefined && pins.size < PROVIDER_LIMIT) {
          pins.set(provider, { provider, hash: entry.hash, lastUsedAt: Math.max(at, latestAssistant.get(provider) ?? 0) });
        } else if (pins.size >= PROVIDER_LIMIT) issues.add('provider-limit');
      }
    } else if (entry.type === 'message' && entry.message?.role === 'assistant') {
      // 본문/오류/사용량은 접근하지 않습니다. pin 이후 timestamp만 원래 manager 의미에 맞춥니다.
      const provider = text(entry.message.provider);
      const at = time(entry.message.timestamp);
      if (provider && at !== undefined && !pins.has(provider) && latestAssistant.size < PROVIDER_LIMIT) {
        latestAssistant.set(provider, Math.max(at, latestAssistant.get(provider) ?? 0));
      }
    }
    id = entry.parentId;
  }
  if (id !== null && id !== undefined && visited.size >= BRANCH_LIMIT) issues.add('branch-scan-limit');
  return [...pins.values()];
}

function rememberCall(calls, call) {
  const key = JSON.stringify([call.provider, call.model, call.purpose, call.role ?? '', call.route ?? 'unknown']);
  const previous = calls.get(key);
  if (previous && previous.recordedAt > call.recordedAt) return;
  calls.delete(key);
  calls.set(key, call);
  if (calls.size > CALL_LIMIT) calls.delete(calls.keys().next().value);
}

function auxiliaryCalls(manager, calls, issues) {
  // 공개 API에 append 구독/페이지 조회가 없습니다. 전체 배열의 얕은 복사만 받고
  // 최근 metadata를 15초에 한 번만 검사합니다. branch 밖 model_usage도 놓치지 않습니다.
  const entries = manager.getEntries();
  const start = Math.max(0, entries.length - USAGE_LIMIT);
  if (start > 0) issues.add('usage-scan-limit');
  for (let index = start; index < entries.length; index += 1) {
    const entry = entries[index];
    if (entry.type === 'custom' && entry.customType === ROUTE_ENTRY) {
      const call = routeCall(entry.data);
      if (call) rememberCall(calls, call);
      continue;
    }
    if (entry.type !== 'model_usage') continue;
    const provider = text(entry.provider);
    const model = text(entry.model);
    const purpose = text(entry.purpose);
    const recordedAt = time(Date.parse(entry.timestamp));
    if (!provider || !model || !purpose || recordedAt === undefined) continue;
    const call = { provider, model, purpose, recordedAt };
    const role = text(entry.role);
    if (role) call.role = role;
    if (STOP_REASONS.has(entry.stopReason)) call.stopReason = entry.stopReason;
    rememberCall(calls, call);
  }
}

/** @param {import('@oh-my-pi/pi-coding-agent').ExtensionAPI} pi */
export default function aamObserver(pi) {
  // 팩터리는 child마다 재호출됩니다. 모듈 전역에 session 상태를 두지 않습니다.
  const startedAt = Date.now();
  const fallback = process.platform === 'win32'
    ? join(process.env.APPDATA || join(homedir(), 'AppData/Roaming'), 'AI Account Manager')
    : join(homedir(), 'Library/Application Support/AI Account Manager');
  const root = resolve(process.env.AAM_HOME || INSTALLED_AAM_HOME || fallback);
  const writer = process.platform === 'win32' ? nativeWriter(root) : undefined;
  let context;
  let timer;
  let sessionId;
  let sessionFile;
  let lifecycle = 'idle';
  let dirty = false;
  let stopped = false;
  let lastWrite = 0;
  let lastUsageScan = 0;
  let lastLeaf;
  let pins = [];
  let pinIssues = [];
  let usageIssues = [];
  const calls = new Map();
  let writing;
  let failedWrite = false;
  let installationWatch;

  async function installationRemoved() {
    if (stopped) return;
    stopped = true;
    installationWatch?.close();
    installationWatch = undefined;
    if (timer) context?.clearTimer(timer);
    timer = undefined;
    await writing;
    await writer?.close();
  }
  if (INSTALLED_AAM_ENTRY) {
    try {
      installationWatch = watch(INSTALLED_AAM_ENTRY, { persistent: false }, event => {
        if (event === 'rename') void installationRemoved();
      });
      installationWatch.on('error', () => { void installationRemoved(); });
    } catch { void installationRemoved(); }
  }

  function adopt(ctx) {
    const nextId = text(ctx.sessionManager.getSessionId());
    const nextFile = text(ctx.sessionManager.getSessionFile(), 4_096);
    if (!nextId || !nextFile || !isAbsolute(nextFile)) return false;
    if (nextId !== sessionId || nextFile !== sessionFile) {
      sessionId = nextId;
      sessionFile = nextFile;
      calls.clear();
      pins = [];
      pinIssues = [];
      usageIssues = [];
      lastLeaf = undefined;
      lastUsageScan = 0;
    }
    context = ctx;
    return true;
  }

  function snapshot() {
    if (!context || !adopt(context)) return undefined;
    const manager = context.sessionManager;
    const observedAt = Date.now();
    const issues = new Set(BASE_ISSUES);
    if (failedWrite) issues.add('previous-write-failed');
    const leafId = manager.getLeafId();
    if (lastLeaf !== leafId) {
      const currentIssues = new Set();
      try { pins = currentPins(manager, currentIssues); }
      catch { pins = []; currentIssues.add('pin-source-unavailable'); }
      pinIssues = [...currentIssues];
      lastLeaf = leafId;
    }
    for (const issue of pinIssues) issues.add(issue);
    if (observedAt - lastUsageScan >= HEARTBEAT_INTERVAL || stopped) {
      const currentIssues = new Set();
      try { auxiliaryCalls(manager, calls, currentIssues); }
      catch { currentIssues.add('usage-source-unavailable'); }
      usageIssues = [...currentIssues];
      lastUsageScan = observedAt;
    }
    for (const issue of usageIssues) issues.add(issue);
    const providers = new Set(pins.map(pin => pin.provider));
    const modelProvider = text(context.model?.provider);
    if (modelProvider) providers.add(modelProvider);
    for (const call of calls.values()) providers.add(call.provider);
    const selections = [];
    if (providers.size > PROVIDER_LIMIT) issues.add('provider-limit');
    for (const provider of [...providers].slice(0, PROVIDER_LIMIT)) {
      try {
        const accounts = context.modelRegistry.authStorage.listOAuthAccounts(provider, sessionId);
        let active;
        let count = 0;
        for (const account of accounts) {
          if (account.active === true) { count += 1; active = account; }
        }
        if (count > 1) { issues.add('selection-ambiguous'); continue; }
        if (count !== 1) continue;
        const hash = identityHash(provider, active);
        if (hash) selections.push({ provider, hash });
        else issues.add('selection-identity-unavailable');
      } catch { issues.add('selection-source-unavailable'); }
    }
    // 선택 관측은 persisted session-key affinity일 뿐입니다. selectedAt도 공개되지
    // 않으므로 생략하며, API-key override/재시도/동시 호출의 계정으로 승격하지 않습니다.
    const result = {
      version: 1, pid: process.pid, sessionId, sessionFile, observedAt, startedAt,
      lifecycle, pins, selections, calls: [...calls.values()], issues: [...issues],
    };
    const parentSessionFile = text(manager.getHeader()?.parentSession, 4_096);
    if (parentSessionFile && isAbsolute(parentSessionFile)) result.parentSessionFile = parentSessionFile;
    if (text(leafId)) result.leafId = leafId;
    return result;
  }

  function flush(force = false) {
    if (stopped && !force) return;
    if (writing) return writing;
    if (!force && (!context || (!dirty && Date.now() - lastWrite < HEARTBEAT_INTERVAL))) return;
    if (!force && Date.now() - lastWrite < WRITE_INTERVAL) return;
    let value;
    try { value = snapshot(); }
    catch { failedWrite = true; return; }
    if (!value) return;
    dirty = false;
    lastWrite = Date.now();
    writing = writeSnapshot(root, value, writer)
      .then(() => { failedWrite = false; })
      .catch(() => { failedWrite = true; })
      .finally(() => { writing = undefined; });
    return writing;
  }

  function observe(ctx, state) {
    try {
      if (stopped || !adopt(ctx)) return;
      if (state) lifecycle = state;
      dirty = true;
      if (!timer) timer = ctx.setInterval(() => { try { return flush(); } catch { failedWrite = true; } }, WRITE_INTERVAL);
    } catch { failedWrite = true; }
  }

  for (const event of ['session_start', 'session_switch', 'session_branch', 'session_tree', 'session_compact']) {
    pi.on(event, (_event, ctx) => {
      try { observe(ctx, ctx.isIdle() ? 'idle' : 'running'); }
      catch { failedWrite = true; }
    });
  }
  pi.on('agent_start', (_event, ctx) => observe(ctx, 'running'));
  pi.on('turn_end', (_event, ctx) => observe(ctx));
  pi.on('agent_end', (event, ctx) => observe(ctx, event.willContinue ? 'running' : 'idle'));
  function observeRoute(ctx) {
    try {
      if (stopped || !adopt(ctx)) return;
      const provider = text(ctx.model?.provider);
      const model = text(ctx.model?.id);
      if (!provider || !model) return;
      const data = { version: 1, provider, model, route: requestRoute(ctx.model), recordedAt: Date.now() };
      // custom entry는 LLM 문맥에 들어가지 않는다. 서비스 재시작·확장 reload 후에도
      // 호출 당시의 경로를 복원하며, 완료 메시지와 시각으로 억지 연결하지 않는다.
      pi.appendEntry(ROUTE_ENTRY, data);
      rememberCall(calls, routeCall(data));
      observe(ctx);
    } catch { failedWrite = true; }
  }
  // Codex WebSocket에는 HTTP 응답 hook이 없고, pi-native에는 payload hook이 없다.
  // 둘 다 관측하되 요청 준비를 응답 완료나 호출 횟수의 증거로 승격하지 않는다.
  pi.on('before_provider_request', (_event, ctx) => { observeRoute(ctx); });
  pi.on('after_provider_response', (_event, ctx) => { observeRoute(ctx); });
  pi.on('message_end', (event, ctx) => {
    try {
      if (stopped || !adopt(ctx) || event.message?.role !== 'assistant') return;
      const provider = text(event.message.provider);
      const model = text(event.message.model);
      if (provider && model) {
        const call = { provider, model, purpose: 'assistant', recordedAt: Date.now() };
        if (STOP_REASONS.has(event.message.stopReason)) call.stopReason = event.message.stopReason;
        rememberCall(calls, call);
      }
      // v18.2.6은 message_end 통지 후 pin을 기록합니다. 여기서 pin을 해당
      // 호출에 붙이지 않고 다음 timer에서 독립적인 branch 이력으로만 관측합니다.
      observe(ctx);
    } catch { failedWrite = true; }
  });
  pi.on('session_shutdown', async (_event, ctx) => {
    try {
      if (stopped) return;
      adopt(ctx);
      stopped = true;
      lifecycle = 'shutdown';
      if (timer) ctx.clearTimer(timer);
      timer = undefined;
      await writing;
      await flush(true);
    } catch { /* 종료/오프라인/권한 오류로 OMP를 방해하지 않습니다. */ }
    finally { installationWatch?.close(); await writer?.close(); }
  });
}
