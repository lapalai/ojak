import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { copyFile, mkdir, mkdtemp, readFile, readdir, rename, rm, stat, symlink, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { dirname, join } from 'node:path';
import test from 'node:test';
import { execFileSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';

const windows = process.platform === 'win32';
const writer = process.env.AAM_OBSERVER_TEST_WRITER || fileURLToPath(new URL('../../target/debug/aam.exe', import.meta.url));
// Exercise the real ACL-aware launcher, with the same injection performed by the installer.
const source = await readFile(new URL('./aam-observer.js', import.meta.url), 'utf8');
const { default: observer } = await import(`data:text/javascript;base64,${Buffer.from(source.replace('const INSTALLED_AAM_WRITER = undefined;', `const INSTALLED_AAM_WRITER = ${JSON.stringify(writer)};`)).toString('base64')}`);
const sid = windows ? execFileSync('powershell.exe', ['-NoProfile', '-NonInteractive', '-Command', '[System.Security.Principal.WindowsIdentity]::GetCurrent().User.Value'], { encoding: 'utf8', windowsHide: true }).trim() : null;

const hash = value => createHash('sha256').update(value).digest('hex');
const inaccessible = () => { throw new Error('비밀/본문 접근 금지'); };

async function environment(t) {
  // AAM_HOME은 미리 만들지 않는다. Windows에서는 writer가 없는 폴더를 자기 규칙(restrict_dir)으로 만들어야
  // 소유자·DACL이 검사와 맞는다. 상위 임시 폴더에 icacls로 ACL을 흉내 내면 러너의 상속·소유자 설정에 따라
  // 사용자 ACE가 반영되지 않아(GitHub Windows 러너: SYSTEM·Administrators만 남음) writer가 unsafe로 거절한다.
  const parent = await mkdtemp(join(tmpdir(), 'aam-observer-'));
  const root = join(parent, 'home');
  const before = process.env.AAM_HOME;
  process.env.AAM_HOME = root;
  t.after(async () => {
    if (before === undefined) delete process.env.AAM_HOME;
    else process.env.AAM_HOME = before;
    await rm(parent, { recursive: true, force: true });
  });
  return root;
}

function session(id, entries = [], accounts = [], factory = observer) {
  const handlers = new Map();
  let tick;
  let leaf = entries.at(-1)?.id ?? null;
  let reads = 0;
  const manager = {
    getSessionId: () => id,
    getSessionFile: () => join(tmpdir(), 'fixture', `${id}.jsonl`),
    getHeader: () => ({ id }),
    getLeafId: () => leaf,
    getEntry: entryId => { reads += 1; return entries.find(entry => entry.id === entryId); },
    getEntries: () => [...entries],
    getCredentialPins: inaccessible,
    getBranch: inaccessible,
  };
  const context = {
    sessionManager: manager,
    model: { provider: 'openai-codex', id: 'model-main' },
    modelRegistry: {
      authStorage: {
        listOAuthAccounts(provider, sessionId) {
          assert.equal(provider, 'openai-codex');
          assert.equal(sessionId, id);
          return accounts;
        },
        getOAuthAccountIdentity: inaccessible,
        getApiKey: inaccessible,
      },
    },
    isIdle: () => true,
    setInterval(callback) { tick = callback; return {}; },
    clearTimer() { tick = undefined; },
  };
  factory({
    on: (name, handler) => handlers.set(name, handler),
    appendEntry: (customType, data) => {
      const entry = { type: 'custom', customType, data, id: `custom-${entries.length}`, parentId: leaf, timestamp: new Date().toISOString() };
      entries.push(entry);
      leaf = entry.id;
    },
  });
  return {
    context,
    emit: (name, event = {}, ctx = context) => handlers.get(name)?.({ ...event, type: name }, ctx),
    tick: () => tick?.(),
    append(entry) { entries.push(entry); leaf = entry.id; },
    reads: () => reads,
    persisted: () => entries.filter(entry => entry.type === 'custom'),
  };
}

async function snapshot(root, id) {
  return JSON.parse(await readFile(join(root, 'omp-observations', `${process.pid}-${hash(id)}.json`), 'utf8'));
}

function pin(id, parentId, value) {
  return { type: 'credential_pin', id, parentId, provider: 'openai-codex', hash: value.repeat(64), timestamp: new Date().toISOString() };
}

test('pin 기록보다 먼저 발생한 message_end도 호출 계정으로 승격하지 않습니다', async t => {
  const root = await environment(t);
  const active = { accountId: 'Account-Raw-Case', email: 'Raw@Example.test', orgId: 'org', projectId: 'project', active: true };
  Object.defineProperty(active, 'access', { get: inaccessible });
  const fixture = session('main', [pin('old-pin', null, 'a')], [active]);
  fixture.emit('session_start');
  const message = { role: 'assistant', provider: 'openai-codex', model: 'model-main', stopReason: 'error' };
  Object.defineProperty(message, 'content', { get: inaccessible });
  Object.defineProperty(message, 'errorMessage', { get: inaccessible });
  fixture.emit('message_end', { message });
  fixture.append(pin('new-pin', 'old-pin', 'b'));
  await fixture.tick();
  const result = await snapshot(root, 'main');
  assert.equal(result.pins[0].hash, 'b'.repeat(64));
  assert.equal(result.selections[0].hash, hash(['openai-codex', active.accountId, active.email, active.orgId, active.projectId].join('\0')));
  assert.equal(Object.hasOwn(result.selections[0], 'selectedAt'), false);
  assert.deepEqual(result.calls.map(({ recordedAt, ...call }) => call), [{ provider: 'openai-codex', model: 'model-main', purpose: 'assistant', stopReason: 'error' }]);
  assert.ok(result.issues.includes('request-account-unavailable'));
  const file = join(root, 'omp-observations', `${process.pid}-${hash('main')}.json`);
  if (windows) {
    const acl = JSON.parse(execFileSync('powershell.exe', ['-NoProfile', '-NonInteractive', '-Command', '$a=Get-Acl -LiteralPath $env:OBSERVER_TEST_FILE; @{ protected=$a.AreAccessRulesProtected; rules=@($a.Access | ForEach-Object { @{sid=$_.IdentityReference.Translate([System.Security.Principal.SecurityIdentifier]).Value; type=$_.AccessControlType.ToString()} }) } | ConvertTo-Json -Depth 4 -Compress'], { encoding: 'utf8', windowsHide: true, env: { ...process.env, OBSERVER_TEST_FILE: file } }));
    assert.ok(acl.rules.length > 0);
    assert.ok(acl.rules.every(rule => rule.type === 'Allow' && rule.sid === sid));
  } else {
    assert.equal((await stat(file)).mode & 0o777, 0o600);
    assert.equal((await stat(join(root, 'omp-observations'))).mode & 0o777, 0o700);
  }
  assert.deepEqual(await readdir(join(root, 'omp-observations')), [`${process.pid}-${hash('main')}.json`]);
  await fixture.emit('session_shutdown');
  assert.equal((await snapshot(root, 'main')).lifecycle, 'shutdown');
});

test('inactive 계정 fallback과 모호한 다중 active를 거부합니다', async t => {
  const root = await environment(t);
  const inactive = session('inactive', [], [{ email: 'first@example.test', active: false }]);
  inactive.emit('session_start');
  await inactive.tick();
  assert.deepEqual((await snapshot(root, 'inactive')).selections, []);
  const ambiguous = session('ambiguous', [], [{ email: 'a@example.test', active: true }, { email: 'b@example.test', active: true }]);
  ambiguous.emit('session_start');
  await ambiguous.tick();
  const result = await snapshot(root, 'ambiguous');
  assert.deepEqual(result.selections, []);
  assert.ok(result.issues.includes('selection-ambiguous'));
});

test('같은 PID의 child 상태와 branch 밖 보조 호출을 독립적으로 보존합니다', async t => {
  const root = await environment(t);
  const auxiliary = { type: 'model_usage', id: 'aux', parentId: null, provider: 'openai-codex', model: 'model-aux', purpose: 'summary', role: 'smol', stopReason: 'stop', timestamp: new Date().toISOString() };
  Object.defineProperty(auxiliary, 'errorMessage', { get: inaccessible });
  Object.defineProperty(auxiliary, 'usage', { get: inaccessible });
  const parent = session('parent', [auxiliary, pin('parent-pin', null, 'a')]);
  const child = session('child', [pin('child-pin', null, 'b')]);
  parent.emit('session_start');
  child.emit('session_start');
  await Promise.all([parent.tick(), child.tick()]);
  assert.equal((await snapshot(root, 'parent')).calls[0].purpose, 'summary');
  assert.equal((await snapshot(root, 'parent')).calls[0].role, 'smol');
  assert.deepEqual((await snapshot(root, 'child')).calls, []);
  assert.equal((await snapshot(root, 'child')).pins[0].hash, 'b'.repeat(64));
});

test('긴 branch는 순회 한도를 지키고 불완전성을 표시합니다', async t => {
  const root = await environment(t);
  const entries = Array.from({ length: 5_000 }, (_, index) => ({ id: String(index), parentId: index === 0 ? null : String(index - 1), type: 'label' }));
  const fixture = session('bounded', entries);
  fixture.emit('session_start');
  await fixture.tick();
  assert.equal(fixture.reads(), 4_096);
  assert.ok((await snapshot(root, 'bounded')).issues.includes('branch-scan-limit'));
});

test('observer 경로 symlink 충돌은 외부 파일을 변경하거나 OMP에 오류를 전파하지 않습니다', async t => {
  const root = await environment(t);
  // 양성 대조: 정상 writer가 AAM_HOME을 자기 규칙으로 만들고 스냅샷을 실제로 쓴다. 이게 통과해야
  // 아래 거부가 symlink 때문이지 폴더 권한 때문이 아님이 확인된다.
  const control = session('control');
  control.emit('session_start');
  await control.tick();
  assert.ok((await snapshot(root, 'control')).lifecycle, 'positive control: writer must create AAM_HOME and persist a snapshot');
  await control.emit('session_shutdown');
  // 관측 폴더만 외부 폴더로 가는 symlink/junction으로 바꾼다. 상위 AAM_HOME은 writer가 만든 안전한 폴더 그대로다.
  const observations = join(root, 'omp-observations');
  await rm(observations, { recursive: true, force: true });
  const outside = await mkdtemp(join(tmpdir(), 'aam-observer-outside-'));
  t.after(() => rm(outside, { recursive: true, force: true }));
  const sentinel = join(outside, 'keep');
  await writeFile(sentinel, '그대로 유지');
  await symlink(outside, observations, windows ? 'junction' : 'dir');
  const fixture = session('collision');
  fixture.emit('session_start');
  await fixture.tick();
  await fixture.emit('session_shutdown');
  assert.equal(await readFile(sentinel, 'utf8'), '그대로 유지');
  assert.deepEqual(await readdir(outside), ['keep']);
});

test('응답 경로는 선택 모델·완료 메시지가 아닌 요청별 model로 기록하고 reload 후 복원합니다', async t => {
  const root = await environment(t);
  const fixture = session('route-session');
  fixture.emit('session_start');
  const response = { status: 200 };
  Object.defineProperty(response, 'headers', { get: inaccessible });
  const model = { provider: 'ojak-antigravity', id: 'gemini', transport: 'pi-native', baseUrl: 'http://127.0.0.1:4020' };
  fixture.emit('after_provider_response', response, { ...fixture.context, model });
  fixture.emit('message_end', { message: { role: 'assistant', provider: 'google-antigravity', model: 'gemini', stopReason: 'stop' } });
  await fixture.tick();
  const calls = (await snapshot(root, 'route-session')).calls;
  assert.equal(calls.find(call => call.route !== undefined).route, 'bridge');
  assert.equal(calls.find(call => call.route !== undefined).provider, 'ojak-antigravity');
  assert.equal(calls.find(call => call.purpose === 'assistant').route, undefined);
  assert.equal(JSON.stringify(fixture.persisted()).includes('http:'), false);
  const restored = session('restored', fixture.persisted());
  restored.emit('session_start');
  await restored.tick();
  assert.equal((await snapshot(root, 'restored')).calls.find(call => call.model === 'gemini').route, 'bridge');
});

test('같은 모델의 직접·경유 경로를 따로 유지하며 잘못된 경로 근거는 확정하지 않습니다', async t => {
  const root = await environment(t);
  const fixture = session('mixed-route');
  fixture.emit('session_start');
  for (const model of [
    { provider: 'anthropic', id: 'shared', baseUrl: 'https://api.anthropic.com' },
    { provider: 'ojak-claude', id: 'shared', baseUrl: 'http://127.0.0.1:4020', transport: 'pi-native' },
    { provider: 'anthropic', id: 'shared', baseUrl: 'https://external-gateway.test', transport: 'pi-native' },
    { provider: 'ojak-claude', id: 'missing-url' },
  ]) fixture.emit('after_provider_response', { status: 200 }, { ...fixture.context, model });
  await fixture.tick();
  const calls = (await snapshot(root, 'mixed-route')).calls;
  assert.deepEqual(calls.filter(call => call.model === 'shared').map(call => call.route).sort(), ['bridge', 'direct', 'unknown']);
  assert.equal(calls.find(call => call.model === 'missing-url').route, 'unknown');
});

test('응답 hook이 없는 WebSocket도 요청 hook으로 경로를 보존하고 payload는 건드리지 않습니다', async t => {
  const root = await environment(t);
  const fixture = session('websocket-route');
  fixture.emit('session_start');
  const payload = {};
  Object.defineProperty(payload, 'input', { get: inaccessible });
  const ctx = { ...fixture.context, model: { provider: 'openai-codex', id: 'gpt-6-astra', baseUrl: 'https://chatgpt.com/backend-api/codex' } };
  assert.equal(fixture.emit('before_provider_request', { payload }, ctx), undefined);
  await fixture.tick();
  const call = (await snapshot(root, 'websocket-route')).calls.find(call => call.route === 'direct');
  assert.equal(call.model, 'gpt-6-astra');
  assert.equal(call.stopReason, undefined);
  assert.equal(fixture.persisted()[0].data.route, 'direct');
});

test('설치된 확장을 제거하면 관측을 멈추고 Windows 실행 파일 잠금을 풀어요', async t => {
  let fixture;
  t.after(() => fixture?.emit('session_shutdown'));
  const root = await environment(t);
  const directory = join(dirname(root), 'installed-observer');
  const entry = join(directory, 'index.js');
  const helper = join(dirname(root), 'aam-writer.exe');
  await mkdir(directory);
  await writeFile(entry, source);
  if (windows) await copyFile(writer, helper);
  const configured = source
    .replace('const INSTALLED_AAM_ENTRY = undefined;', `const INSTALLED_AAM_ENTRY = ${JSON.stringify(entry)};`)
    .replace('const INSTALLED_AAM_WRITER = undefined;', `const INSTALLED_AAM_WRITER = ${JSON.stringify(helper)};`);
  const { default: installedObserver } = await import(`data:text/javascript;base64,${Buffer.from(configured).toString('base64')}`);
  fixture = session('removed-extension', [], [], installedObserver);
  let stopped;
  const stoppedTimer = new Promise(resolve => { stopped = resolve; });
  const clearTimer = fixture.context.clearTimer;
  fixture.context.clearTimer = (...args) => { clearTimer(...args); stopped(); };
  fixture.emit('session_start');
  await fixture.tick();
  const before = await snapshot(root, 'removed-extension');
  const staging = join(dirname(root), 'removed-observer');
  let timeout;
  try {
    const deadline = new Promise((_, reject) => { timeout = setTimeout(() => reject(new Error('observer-did-not-stop')), 5_000); });
    await rename(directory, staging);
    await rm(join(staging, 'index.js'));
    await Promise.race([stoppedTimer, deadline]);
    await fixture.emit('session_shutdown');
    fixture.emit('message_end', { message: { role: 'assistant', provider: 'openai-codex', model: 'after-uninstall', stopReason: 'stop' } });
    await fixture.tick();
    assert.deepEqual(await snapshot(root, 'removed-extension'), before);
    if (windows) await rm(helper);
  } finally { clearTimeout(timeout); }
});
