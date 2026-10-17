import assert from 'node:assert/strict';
import test from 'node:test';
import {
  bridgedModels,
  connectedAccounts,
  createOutageReporter,
  fetchBridgeModels,
  isConnectionRefused,
  probeBridge,
  projectModels,
  registerOutageHooks,
  isAdmissionRejection,
  waitForAdmission,
  lastAdmission,
} from './aam-accounts.js';

const rows = [
  { id: 'openai-codex/gpt-6-astra', owned_by: 'openai-codex', display_name: 'GPT-6 Astra', context_length: 272000, max_output_tokens: 128000, input_modalities: ['text', 'image'] },
  { id: 'anthropic/claude-fable-5-1', owned_by: 'anthropic', context_length: 200000 },
  { id: 'openai-codex/gpt-image-2', owned_by: 'openai-codex' },
];

test('bridge rows become the Ojak provider roster without the upstream prefix', () => {
  const models = bridgedModels(rows, 'openai-codex');
  assert.deepEqual(models.map(model => model.id), ['gpt-6-astra', 'gpt-image-2']);
  assert.equal(models[0].contextWindow, 272000);
  assert.deepEqual(models[0].input, ['text', 'image']);
  // 다른 공급자 행과 잘못된 응답은 무시한다.
  assert.deepEqual(bridgedModels(null, 'openai-codex'), []);
});

test('upstream catalog rows win when present, and bridge rows survive when upstream has not loaded yet', () => {
  const bridged = bridgedModels(rows, 'openai-codex').map(model => ({ ...model, provider: 'ojak-codex' }));
  // 시작 직후: 원래 공급자에는 이미지 모델뿐. 채팅 모델은 브릿지 목록으로 남아야 한다(이번 버그).
  const early = projectModels([{ id: 'gpt-image-2', provider: 'openai-codex', cost: { input: 1 } }, ...bridged], 'ojak-codex', 'openai-codex');
  const ojak = early.filter(model => model.provider === 'ojak-codex');
  assert.deepEqual(ojak.map(model => model.id).sort(), ['gpt-6-astra', 'gpt-image-2']);
  assert.deepEqual(ojak.find(model => model.id === 'gpt-image-2').cost, { input: 1 });
  // 원래 공급자 목록이 다 들어오면 같은 id는 원래 공급자 정보로 바뀐다.
  const late = projectModels([{ id: 'gpt-6-astra', provider: 'openai-codex', thinking: ['high'] }, ...bridged], 'ojak-codex', 'openai-codex');
  assert.deepEqual(late.filter(model => model.provider === 'ojak-codex' && model.id === 'gpt-6-astra').map(model => model.thinking), [['high']]);
});

const refused = () => Object.assign(new TypeError('fetch failed'), { cause: Object.assign(new Error('connect ECONNREFUSED 127.0.0.1:4020'), { code: 'ECONNREFUSED' }) });

test('only a refused TCP connection counts as a refusal, however the runtime reports it', () => {
  assert.equal(isConnectionRefused(refused()), true);
  assert.equal(isConnectionRefused(Object.assign(new Error('Unable to connect'), { code: 'ConnectionRefused' })), true);
  assert.equal(isConnectionRefused('connect ECONNREFUSED 127.0.0.1:4020'), true);
  assert.equal(isConnectionRefused(Object.assign(new Error('The operation timed out'), { name: 'TimeoutError' })), false);
  assert.equal(isConnectionRefused(Object.assign(new Error('Unable to connect'), { code: 'ETIMEDOUT' })), false);
  assert.equal(isConnectionRefused(new Error('Unable to connect')), false);
  assert.equal(isConnectionRefused(new Error('auth-gateway 401: unauthorized')), false);
  assert.equal(isConnectionRefused(undefined), false);
});

test('probe treats any HTTP answer as reachable and only a refusal as down', async () => {
  const answer = status => async () => new Response('{}', { status });
  assert.equal(await probeBridge(answer(200)), 'up');
  assert.equal(await probeBridge(answer(401)), 'up');
  assert.equal(await probeBridge(answer(503)), 'up');
  assert.equal(await probeBridge(async () => { throw refused(); }), 'down');
  assert.equal(await probeBridge(async () => { throw Object.assign(new Error('timeout'), { name: 'TimeoutError' }); }), 'unknown');
});

test('bridge requests attribute refusals to the bridge and leave every other failure alone', async () => {
  const original = refused();
  await assert.rejects(connectedAccounts('t', 'anthropic', async () => { throw original; }), error => {
    assert.equal(error.name, 'BridgeUnreachableError');
    assert.equal(error.cause, original);
    return true;
  });
  await assert.rejects(fetchBridgeModels('t', 'anthropic', async () => { throw original; }), error => error.name === 'BridgeUnreachableError');
  const other = new Error('socket hang up');
  await assert.rejects(connectedAccounts('t', 'anthropic', async () => { throw other; }), error => error === other);
  // HTTP 응답은 브릿지에 닿은 것이다. 인증 거절은 연결 문제로 둔갑하지 않는다.
  await assert.rejects(connectedAccounts('t', 'anthropic', async () => new Response('no', { status: 401 })), error => error.name === 'Error' && /401/.test(error.message));
  assert.equal(await connectedAccounts('t', 'anthropic', async () => Response.json({ providers: [{ provider: 'anthropic', accounts: 2 }] })), 2);
});

function hooks(states) {
  const handlers = new Map();
  const pi = { on: (name, handler) => handlers.set(name, handler) };
  const notices = [];
  let probes = 0;
  const outage = createOutageReporter(async () => { probes += 1; return typeof states === 'function' ? states() : states; });
  registerOutageHooks(pi, outage);
  const ctx = { model: { provider: 'ojak-claude' }, ui: { notify: (message, level) => notices.push({ message, level }) } };
  const fire = (name, event, context = ctx) => handlers.get(name)(event, context);
  const flush = async () => { for (let i = 0; i < 4; i += 1) await new Promise(resolve => setImmediate(resolve)); };
  return { fire, flush, notices, probes: () => probes };
}
const failed = (errorMessage = 'connect ECONNREFUSED 127.0.0.1:4020', stopReason = 'error') => ({ message: { role: 'assistant', provider: 'ojak-claude', stopReason, errorMessage } });

test('one outage notifies once however many failures and retries report it', async () => {
  const h = hooks('down');
  h.fire('message_end', failed());
  h.fire('message_end', failed());
  h.fire('auto_retry_start', { errorMessage: 'connect ECONNREFUSED 127.0.0.1:4020' });
  await h.flush();
  h.fire('message_end', failed());
  await h.flush();
  assert.equal(h.notices.length, 1);
  assert.equal(h.notices[0].level, 'error');
  assert.equal(h.probes(), 1);
});

test('a real response ends the outage so the next one is reported again', async () => {
  const h = hooks('down');
  h.fire('message_end', failed());
  await h.flush();
  h.fire('after_provider_response', { status: 401 });
  h.fire('message_end', failed());
  await h.flush();
  assert.equal(h.notices.length, 2);
  // 성공한 응답과 omp 재시도 성공도 복구로 센다.
  h.fire('message_end', { message: { role: 'assistant', provider: 'ojak-claude', stopReason: 'stop' } });
  h.fire('message_end', failed());
  await h.flush();
  assert.equal(h.notices.length, 3);
  h.fire('auto_retry_end', { success: true });
  h.fire('message_end', failed());
  await h.flush();
  assert.equal(h.notices.length, 4);
});

test('a refusal-looking error is not blamed on the bridge unless the bridge is really refusing', async () => {
  for (const state of ['up', 'unknown']) {
    const h = hooks(state);
    h.fire('message_end', failed('auth-gateway 502: upstream connection refused'));
    await h.flush();
    assert.deepEqual(h.notices, []);
  }
  // 'up'은 복구 증거다. 이어서 정말 꺼지면 새 중단으로 알린다.
  let state = 'up';
  const h = hooks(() => state);
  h.fire('message_end', failed());
  await h.flush();
  state = 'down';
  h.fire('message_end', failed());
  await h.flush();
  assert.equal(h.notices.length, 1);
});

test('cancellation, other errors and non-Ojak models never reach the probe', async () => {
  const h = hooks('down');
  h.fire('message_end', failed('connect ECONNREFUSED 127.0.0.1:4020', 'aborted'));
  h.fire('message_end', failed('auth-gateway 401: unauthorized'));
  h.fire('message_end', failed('rate_limit_error: slow down'));
  h.fire('message_end', { message: { role: 'user', provider: 'ojak-claude', stopReason: 'error', errorMessage: 'ECONNREFUSED' } });
  h.fire('message_end', { message: { role: 'assistant', provider: 'anthropic', stopReason: 'error', errorMessage: 'ECONNREFUSED' } }, { model: { provider: 'anthropic' }, ui: { notify: () => assert.fail('notified') } });
  h.fire('auto_retry_start', { errorMessage: 'ECONNREFUSED' }, { model: { provider: 'openai-codex' }, ui: { notify: () => assert.fail('notified') } });
  await h.flush();
  assert.equal(h.probes(), 0);
  assert.deepEqual(h.notices, []);
});

test('nothing is shown after the session shuts down, and a throwing UI cannot break the hook', async () => {
  const h = hooks('down');
  h.fire('message_end', failed(), { model: { provider: 'ojak-codex' }, ui: { notify: () => { throw new Error('ui gone'); } } });
  await h.flush();
  h.fire('session_shutdown', {});
  h.fire('after_provider_response', {});
  h.fire('message_end', failed());
  await h.flush();
  assert.deepEqual(h.notices, []);
});

test('only an explicit pre-upstream admission rejection permits replay', () => {
  const error = { type: 'admission_wait', code: 'BRIDGE_RESOURCE_PRESSURE', submitted: false };
  assert.equal(isAdmissionRejection(503, { error }), true);
  assert.equal(isAdmissionRejection(503, { error: { ...error, submitted: true } }), false);
  assert.equal(isAdmissionRejection(503, { error: { ...error, submitted: undefined } }), false);
  assert.equal(isAdmissionRejection(502, { error }), false);
  assert.equal(isAdmissionRejection(503, { error: { ...error, type: 'upstream_error' } }), false);
  assert.equal(isAdmissionRejection(503, { error: { ...error, code: 'BRIDGE_EXECUTION_UNKNOWN' } }), false);
});

test('cancellation during readiness response cannot grant admission and frees the waiter', async () => {
  for (let attempt = 0; attempt < 33; attempt++) {
    const controller = new AbortController();
    const reason = new Error('cancelled by caller');
    await assert.rejects(waitForAdmission({
      token: 'test', signal: controller.signal,
      fetchImpl: async () => {
        controller.abort(reason);
        return Response.json({ version: 1, ready: true });
      },
    }), error => error === reason);
  }
});

test('pressure wait is cancellable without another network attempt', async () => {
  const controller = new AbortController();
  let requests = 0;
  await assert.rejects(waitForAdmission({
    token: 'test', signal: controller.signal,
    fetchImpl: async () => { requests++; return Response.json({ version: 1, ready: false, reason: 'resource_pressure' }); },
    onWait: () => controller.abort(new Error('stop')),
  }), /stop/);
  assert.equal(requests, 1);
});

test('permanent admission failure and expired wait do not enter a retry loop', async () => {
  let requests = 0;
  const fetchImpl = async () => { requests++; return new Response('', { status: 401 }); };
  await assert.rejects(waitForAdmission({ token: 'test', fetchImpl }), error => error.code === 'BRIDGE_ADMISSION_STOPPED');
  assert.equal(requests, 1);
  await assert.rejects(waitForAdmission({ token: 'test', fetchImpl, deadline: 0 }), error => error.code === 'BRIDGE_ADMISSION_PAUSED');
  assert.equal(requests, 1);
});

test('queued data budget refuses excess work and cancellation returns the reservation', async () => {
  const controller = new AbortController();
  let release;
  const pending = waitForAdmission({
    token: 'test', bytes: 256 * 1024 * 1024, signal: controller.signal,
    fetchImpl: () => new Promise(resolve => { release = resolve; }),
  });
  let sent = false;
  await assert.rejects(waitForAdmission({
    token: 'test', bytes: 1, fetchImpl: async () => { sent = true; return Response.json({ version: 1, ready: true }); },
  }), error => error.code === 'BRIDGE_ADMISSION_PAUSED');
  assert.equal(sent, false);
  controller.abort(new Error('release reservation'));
  release(Response.json({ version: 1, ready: true }));
  await assert.rejects(pending, /release reservation/);
  let admitted = 0;
  await waitForAdmission({
    token: 'test', bytes: 256 * 1024 * 1024,
    fetchImpl: async () => { admitted++; return Response.json({ version: 1, ready: true }); },
  });
  assert.equal(admitted, 1);
});

test('forks and old tree branches cannot resurrect a submitted or completed job', () => {
  const queued = { id: 'entry-queued', type: 'custom', customType: 'ojak.admission.v1', data: { id: 'job', sessionId: 'owner', phase: 'queued' } };
  const current = (sessionId, branch, all = branch) => ({
    sessionManager: { getSessionId: () => sessionId, getBranch: () => branch, getEntries: () => all },
  });
  assert.equal(lastAdmission(current('owner', [queued])).phase, 'queued');
  assert.equal(lastAdmission(current('fork', [queued])), undefined);
  for (const phase of ['submitted', 'unknown', 'cancelled', 'completed']) {
    const terminal = { ...queued, id: `entry-${phase}`, data: { ...queued.data, phase } };
    assert.equal(lastAdmission(current('owner', [queued], [queued, terminal])), undefined);
    assert.equal(lastAdmission(current('owner', [queued, terminal])).phase, phase);
  }
  const newUser = { type: 'message', message: { role: 'user', content: 'new work' } };
  assert.equal(lastAdmission(current('owner', [queued, newUser])), undefined);
});
