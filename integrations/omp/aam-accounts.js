// Ojak(오작) — omp에 Ojak 공급자를 등록한다. (OMP v18.8 ExtensionAPI)
//
// 기존 CLI·omp 로그인이 연결되면 서비스가 Ojak 공급자를 자동 등록한다. /login은 수동 연결 경로다.
// 모델 목록은 두 겹이다. ① `fetchDynamicModels`가 Ojak 브릿지에서 실제 계정의 모델을 직접 받는다(캐시됨).
// 원래 공급자의 목록이 늦게 와도 Ojak 모델이 비지 않는다. ② `modifyModels`는 원래 공급자 행이 있으면
// 그 정보(thinking 단계·비용)로 덮어쓴다. 요청은 pi-native 형식으로 Ojak 브릿지에 보내고, 계정은 Ojak이 고른다.
import { readFileSync } from 'node:fs';
import { homedir } from 'node:os';
import { join } from 'node:path';
import { randomUUID } from 'node:crypto';

const INSTALLED_AAM_HOME = undefined;
const BRIDGE = 'http://127.0.0.1:4020';
// [Ojak 공급자, 원래 공급자, /login 표시 이름]. 브릿지(crates/service/src/bridge.rs ALIASES)와 같은 표를 쓴다.
const PROVIDERS = [
  ['ojak-claude', 'anthropic', 'Ojak · Claude'],
  ['ojak-codex', 'openai-codex', 'Ojak · Codex'],
  ['ojak-antigravity', 'google-antigravity', 'Ojak · Antigravity'],
  ['ojak-grok', 'xai-oauth', 'Ojak · Grok'],
  ['ojak-zai', 'zai', 'Ojak · Z.AI'],
];
// 브릿지 토큰은 만료되지 않는다. omp가 갱신을 시도하지 않도록 먼 만료 시각을 준다.
const LIFETIME_MS = 10 * 365 * 24 * 60 * 60 * 1000;
const ADMISSION_API = 'ojak-durable-native';
const ADMISSION_ENTRY = 'ojak.admission.v1';
const WAIT_LIMIT_MS = 15 * 60_000;
const MAX_WAITERS = 32;
const MAX_WAIT_BYTES = 256 * 1024 * 1024;
let waitingBytes = 0;
const liveContexts = new Map();
const waiters = new Set();

function admissionError(code, message) {
  const error = new Error(`[${code}] ${message}`);
  error.code = code;
  error.status = 400;
  return error;
}

// Bound retained JSON data without creating a second serialized request under
// pressure. Six bytes per UTF-8 byte covers JSON escaping; this is a data
// budget, not a claim about the JS heap's object overhead.
function queuedDataBound(value) {
  let bytes = 0;
  const visit = item => {
    if (bytes > MAX_WAIT_BYTES) return;
    if (typeof item === 'string') { bytes += 2 + 6 * Buffer.byteLength(item); return; }
    if (!item || typeof item !== 'object') { bytes += 24; return; }
    bytes += 2;
    if (Array.isArray(item)) {
      for (const child of item) { visit(child); bytes++; if (bytes > MAX_WAIT_BYTES) break; }
    } else {
      for (const key of Object.keys(item)) {
        bytes += 4 + 6 * Buffer.byteLength(key);
        visit(item[key]);
        if (bytes > MAX_WAIT_BYTES) break;
      }
    }
  };
  visit(value);
  return bytes;
}

function abortableDelay(ms, signal) {
  signal?.throwIfAborted();
  return new Promise((resolve, reject) => {
    const finish = () => { signal?.removeEventListener('abort', abort); resolve(); };
    const timer = setTimeout(finish, ms);
    const abort = () => { clearTimeout(timer); signal.removeEventListener('abort', abort); reject(signal.reason); };
    signal?.addEventListener('abort', abort, { once: true });
  });
}

// Polling is a readiness hint, never a reservation. Only explicit pre-upstream
// rejection permits another POST; a lost POST response is not retried.
export async function waitForAdmission({ signal, token, fetchImpl = fetch, onWait = () => {}, deadline = Date.now() + WAIT_LIMIT_MS, bytes = 0 }) {
  const key = Symbol();
  if (waiters.size >= MAX_WAITERS || bytes > MAX_WAIT_BYTES - waitingBytes) {
    throw admissionError('BRIDGE_ADMISSION_PAUSED', '대기열이 가득 찼어요. 작업은 저장되어 있어요. /ojak-resume으로 이어갈 수 있어요.');
  }
  waiters.add(key);
  waitingBytes += bytes;
  try {
    for (;;) {
      signal?.throwIfAborted();
      if (Date.now() >= deadline) {
        throw admissionError('BRIDGE_ADMISSION_PAUSED', '15분 동안 자리가 나지 않아 대기를 멈췄어요. 작업은 저장되어 있어요. /ojak-resume으로 이어갈 수 있어요.');
      }
      let response;
      try {
        response = await fetchImpl(`${BRIDGE}/v1/admission`, {
          headers: { Authorization: `Bearer ${token}` },
          signal: signal ? AbortSignal.any([signal, AbortSignal.timeout(PROBE_TIMEOUT_MS)]) : AbortSignal.timeout(PROBE_TIMEOUT_MS),
        });
      } catch {
        signal?.throwIfAborted();
        onWait('서비스 연결 대기');
        await abortableDelay(Math.min(2000, Math.max(0, deadline - Date.now())), signal);
        continue;
      }
      if (!response.ok) {
        let body;
        try { body = await response.json(); } catch {}
        if (isAdmissionRejection(response.status, body)) {
          onWait('서비스 연결 자리 대기');
          await abortableDelay(Math.min(2000, Math.max(0, deadline - Date.now())), signal);
          continue;
        }
        throw admissionError('BRIDGE_ADMISSION_STOPPED', `Ojak 대기 상태를 확인하지 못했어요(HTTP ${response.status}). 연결 설정을 확인해 주세요.`);
      }
      const status = await response.json();
      signal?.throwIfAborted();
      if (status.version !== 1 || typeof status.ready !== 'boolean') {
        throw admissionError('BRIDGE_ADMISSION_STOPPED', 'Ojak 서비스와 클라이언트를 함께 업데이트해 주세요.');
      }
      if (status.ready) return;
      onWait(status.reason === 'resource_pressure' ? '메모리 여유 대기' : status.reason === 'resource_unknown' ? '자원 상태 확인 대기' : '모델 연결 자리 대기');
      await abortableDelay(Math.min(2000 + Math.floor(Math.random() * 250), Math.max(0, deadline - Date.now())), signal);
    }
  } finally {
    waiters.delete(key);
    waitingBytes -= bytes;
  }
}

export function isAdmissionRejection(status, body) {
  return status === 503 && body?.error?.type === 'admission_wait' && body.error.submitted === false
    && ['BRIDGE_BUSY', 'BRIDGE_QUEUE_TIMEOUT', 'BRIDGE_RESOURCE_PRESSURE'].includes(body.error.code);
}

export function lastAdmission(ctx) {
  const branch = ctx.sessionManager.getBranch();
  for (let index = branch.length - 1; index >= 0; index--) {
    const entry = branch[index];
    if (entry.type === 'custom' && entry.customType === ADMISSION_ENTRY) {
      if (entry.data?.sessionId !== ctx.sessionManager.getSessionId()) return undefined;
      // A fork/tree jump must not resurrect an old queued marker after the
      // same job was submitted, cancelled, or completed on another branch.
      const entries = ctx.sessionManager.getEntries();
      for (let at = entries.length - 1; at >= 0; at--) {
        const latest = entries[at];
        if (latest.type === 'custom' && latest.customType === ADMISSION_ENTRY && latest.data?.id === entry.data.id) {
          return latest.id === entry.id ? entry.data : undefined;
        }
      }
      return undefined;
    }
    // Never resume a marker belonging to an older user turn.
    if (entry.type === 'message' && entry.message?.role === 'user') return undefined;
  }
}

function durableNativeStream(ai, model, context, options = {}) {
  const output = new ai.AssistantMessageEventStream();
  const binding = [...liveContexts.values()].find(({ ctx }) => ctx.providerSessionId === options.sessionId);
  (async () => {
    const ephemeral = options.requestKind === 'ephemeral';
    if (!ephemeral && (options.requestKind !== 'primary' || !options.sessionId || !binding || typeof binding.ctx.persistSession !== 'function')) {
      throw admissionError('BRIDGE_ADMISSION_STOPPED', '작업 보존 기능이 있는 omp 수정본과 현재 작업 소유권이 필요해요. 요청은 보내지 않았어요.');
    }
    const { pi, ctx } = ephemeral ? {} : binding;
    const previous = ctx && lastAdmission(ctx);
    const job = { id: previous?.phase === 'queued' ? previous.id : randomUUID(), sessionId: ctx?.sessionManager.getSessionId(), provider: model.provider, model: model.id, phase: 'queued', createdAt: Date.now() };
    let submitted = false;
    const save = async phase => {
      job.phase = phase;
      if (ephemeral) return;
      try {
        if (ctx.providerSessionId !== options.sessionId || ctx.sessionManager.getSessionId() !== job.sessionId) throw new Error('Owner changed');
        pi.appendEntry(ADMISSION_ENTRY, { ...job, updatedAt: Date.now() });
        await ctx.persistSession();
      } catch {
        throw admissionError(submitted ? 'BRIDGE_EXECUTION_UNKNOWN' : 'BRIDGE_ADMISSION_STOPPED', '작업 상태를 안전하게 저장하지 못해 자동 진행을 멈췄어요.');
      }
    };
    const showWait = reason => ctx?.ui?.setStatus('ojak-admission', `Ojak: ${reason} · 작업 저장됨 · 취소 가능`);
    const deadline = Date.now() + WAIT_LIMIT_MS;
    try {
      await save('queued');
      const fetchImpl = options.fetch ?? fetch;
      const bytes = queuedDataBound(context);
      const wait = () => waitForAdmission({ signal: options.signal, token: bridgeToken(), fetchImpl, onWait: showWait, deadline, bytes });
      // Delay serialization/native transport allocation until readiness is observed.
      await output.trackLocalWork(wait());
      const inner = ai.streamSimple({ ...model, transport: 'pi-native' }, context, {
        ...options,
        fetch: async (url, init) => {
          const headers = new Headers(init.headers);
          headers.set('X-Ojak-Admission', '1');
          const attemptInit = { ...init, headers };
          for (;;) {
            options.signal?.throwIfAborted();
            await save('submitted');
            submitted = true;
            const response = await fetchImpl(url, attemptInit);
            if (response.status === 503) {
              let body;
              try { body = await response.clone().json(); } catch {}
              if (isAdmissionRejection(response.status, body)) {
                await response.body?.cancel();
                submitted = false;
                await save('queued');
                showWait('모델 연결 자리 대기');
                await output.trackLocalWork(abortableDelay(2000 + Math.floor(Math.random() * 250), options.signal));
                await output.trackLocalWork(wait());
                continue;
              }
              if (body?.error?.type === 'admission_stopped' && body.error.code === 'BRIDGE_ADMISSION_STOPPED' && body.error.submitted === false) {
                await response.body?.cancel();
                submitted = false;
                throw admissionError('BRIDGE_ADMISSION_STOPPED', body.error.message);
              }
            }
            if (!response.ok && response.status < 500) {
              // An explicit authentication/configuration/quota rejection is not
              // resource waiting. Preserve native handling and original status.
              submitted = false;
              await save('rejected');
            }
            ctx?.ui?.setStatus('ojak-admission', undefined);
            return response;
          }
        },
      });
      output.forwardLocalWorkFrom(inner);
      for await (const event of inner) {
        if (event.type === 'done') {
          await save('completed');
        } else if (event.type === 'error') {
          if (options.signal?.aborted) await save('cancelled');
          else if (submitted) {
            await save('unknown');
            event.error.errorType = 'execution_unknown';
            event.error.errorCode = 'BRIDGE_EXECUTION_UNKNOWN';
            event.error.errorMessage = '[BRIDGE_EXECUTION_UNKNOWN] 응답 완료를 확인하지 못해 자동으로 다시 보내지 않았어요.';
            event.error.errorClassificationMessage = event.error.errorMessage;
          }
          else if (job.phase !== 'rejected') {
            const paused = event.error.errorCode === 'BRIDGE_ADMISSION_PAUSED' || event.error.errorMessage?.startsWith('[BRIDGE_ADMISSION_PAUSED]');
            await save(paused ? 'paused' : 'stopped');
          }
        }
        output.push(event);
      }
      output.end();
    } catch (error) {
      if (options.signal?.aborted) {
        await save('cancelled');
        throw error;
      }
      if (submitted) {
        await save('unknown');
        throw admissionError('BRIDGE_EXECUTION_UNKNOWN', '전달한 요청의 완료 여부를 확인할 수 없어 자동으로 다시 보내지 않았어요.');
      }
      if (job.phase !== 'rejected') await save(error?.code === 'BRIDGE_ADMISSION_PAUSED' ? 'paused' : 'stopped');
      throw error;
    } finally {
      ctx?.ui?.setStatus('ojak-admission', undefined);
    }
  })().catch(error => output.fail(error));
  return output;
}

function registerAdmissionHooks(pi) {
  const bind = ctx => {
    liveContexts.set(ctx.sessionManager.getSessionId(), { pi, ctx });
  };
  pi.on('session_start', (_event, ctx) => {
    bind(ctx);
    const pending = lastAdmission(ctx);
    if (pending?.phase === 'queued' && isBridgeProvider(pending.provider) && pending.provider === ctx.model?.provider && pending.model === ctx.model?.id) {
      // Session startup must finish before a continuation re-enters agent events.
      setTimeout(() => {
        if (!liveContexts.has(ctx.sessionManager.getSessionId()) || !ctx.isIdle()) return;
        ctx.resumePendingTurn?.().then(resumed => {
          if (!resumed) ctx.ui?.notify('저장된 대기 작업을 자동 재개하지 못했어요. /ojak-resume을 사용해 주세요.', 'warning');
        }).catch(() => ctx.ui?.notify('저장된 대기 작업을 자동 재개하지 못했어요. /ojak-resume을 사용해 주세요.', 'error'));
      }, 0);
    } else if (pending?.phase === 'submitted' || pending?.phase === 'unknown') {
      ctx.ui?.notify('이전 요청의 실행 결과가 불확실해 자동으로 다시 보내지 않았어요. 결과를 먼저 확인해 주세요.', 'warning');
    }
  });
  pi.on('before_agent_start', (_event, ctx) => { bind(ctx); });
  pi.on('session_shutdown', (_event, ctx) => { liveContexts.delete(ctx.sessionManager.getSessionId()); });
  pi.on('before_subagent_admission', async (event, ctx) => {
    if (!isBridgeProvider(event.model?.split('/')[0] ?? ctx.model?.provider)) return;
    await ctx.persistSession();
    try {
      await waitForAdmission({ signal: event.signal, token: bridgeToken(), onWait: reason => ctx.ui?.setStatus('ojak-admission', `Ojak: agent 생성 전 ${reason}`) });
    } finally {
      ctx.ui?.setStatus('ojak-admission', undefined);
    }
  });
  pi.registerCommand('ojak-resume', {
    description: '저장된 미전송 Ojak 작업의 대기를 다시 시작',
    handler: async (_args, ctx) => {
      const pending = lastAdmission(ctx);
      if (!ctx.isIdle() || !pending || !['queued', 'paused'].includes(pending.phase)) {
        ctx.ui?.notify('재개할 미전송 대기 작업이 없어요. 실행 결과가 불확실한 요청은 자동 재전송하지 않아요.', 'warning');
        return;
      }
      pi.appendEntry(ADMISSION_ENTRY, { ...pending, phase: 'queued', updatedAt: Date.now() });
      await ctx.persistSession();
      if (!(await ctx.resumePendingTurn())) ctx.ui?.notify('저장된 작업을 안전하게 이어갈 수 없어요. 세션 상태를 확인해 주세요.', 'error');
    },
  });
}

function aamHome() {
  return INSTALLED_AAM_HOME ?? process.env.AAM_HOME ?? join(homedir(), 'Library/Application Support/AI Account Manager');
}

function bridgeToken() {
  try {
    const token = readFileSync(join(aamHome(), 'bridge.token'), 'utf8').trim();
    if (token) return token;
  } catch {}
  throw new Error("Ojak 계정 연결이 꺼져 있어요. Ojak 앱의 연결에서 'omp 계정 연결'을 켜 주세요.");
}

// 연결 거절만 로컬 브릿지 문제로 본다. 시간 초과·TLS·HTTP 응답 같은 다른 실패는 원래 오류 그대로 둔다.
const REFUSED_CODES = new Set(['ECONNREFUSED', 'ConnectionRefused']);
const REFUSED_TEXT = /ECONNREFUSED|ConnectionRefused|connection refused/i;
const PROBE_TIMEOUT_MS = 2_000;
const BRIDGE_HOST = new URL(BRIDGE).host;
// 이름을 바꾸기 전 공급자 ID(`aam-*`)로 저장된 세션의 메시지도 같은 브릿지를 쓴다.
const BRIDGE_PROVIDER_NAMES = new Set(PROVIDERS.flatMap(([name]) => [name, name.replace(/^ojak-/, 'aam-')]));

export function bridgeUnreachableMessage() {
  return `Ojak 로컬 브릿지(${BRIDGE_HOST})에 연결하지 못했어요. 이번 연결 오류는 계정 인증·사용량 검사 전에 발생했어요. \`aam service status\`로 서비스가 실행 중인지 확인하고, 꺼져 있으면 Ojak 앱을 열거나 \`aam service install\`로 시작해 주세요.`;
}

/// 오류 객체(와 `cause` 사슬)가 TCP 연결 거절인지. Node는 `cause.code`, Bun은 `code`로 알린다.
export function isConnectionRefused(error) {
  for (let depth = 0, current = error; current && depth < 4; depth += 1, current = current.cause) {
    if (typeof current === 'string' ? REFUSED_TEXT.test(current) : REFUSED_CODES.has(current.code) || (typeof current.message === 'string' && REFUSED_TEXT.test(current.message))) return true;
  }
  return false;
}

export function isBridgeProvider(provider) {
  return typeof provider === 'string' && BRIDGE_PROVIDER_NAMES.has(provider);
}

class BridgeUnreachableError extends Error {
  constructor(cause) {
    super(bridgeUnreachableMessage(), { cause });
    this.name = 'BridgeUnreachableError';
  }
}

/// 인증 없이 `/healthz`만 부른다. 어떤 HTTP 응답이든 브릿지가 살아 있다는 뜻이다. 거절이면 `down`,
/// 시간 초과 같은 불분명한 실패는 `unknown`이라 사용자에게 브릿지 탓이라고 말하지 않는다.
export async function probeBridge(fetchImpl = fetch, timeoutMs = PROBE_TIMEOUT_MS) {
  try {
    const response = await fetchImpl(`${BRIDGE}/healthz`, { signal: AbortSignal.timeout(timeoutMs) });
    try { await response.body?.cancel(); } catch {}
    return 'up';
  } catch (error) {
    return isConnectionRefused(error) ? 'down' : 'unknown';
  }
}

/// 중단 하나당 알림 한 번. 알린 뒤에는 실제 복구(브릿지가 응답함)가 확인될 때까지 같은 중단을 다시 알리지 않는다.
/// 복구 뒤 다시 꺼지면 새 중단이다. 프로브는 동시에 하나만 돌고, 어떤 요청도 다시 보내거나 인증에 손대지 않는다.
export function createOutageReporter(probe = probeBridge) {
  let active = false;
  let epoch = 0;
  let pending;
  return {
    recovered() {
      active = false;
      epoch += 1;
    },
    /// 연결 거절로 보이는 실패가 났을 때. 브릿지가 실제로 꺼져 있고 아직 알리지 않았을 때만 `notify`를 부른다.
    async failure(notify) {
      if (active) return false;
      const mine = epoch;
      if (!pending || pending.epoch !== mine) {
        const current = { epoch: mine, result: probe() };
        pending = current;
        current.result.then(() => { if (pending === current) pending = undefined; }, () => { if (pending === current) pending = undefined; });
      }
      let state;
      try { state = await pending.result; } catch { return false; }
      if (mine !== epoch) return false;
      if (state === 'up') {
        // 브릿지가 응답한다: 이전 중단은 끝났다. 이번 오류는 다른 원인이라 여기서 설명하지 않는다.
        active = false;
        return false;
      }
      if (state !== 'down' || active) return false;
      active = true;
      notify(bridgeUnreachableMessage());
      return true;
    },
  };
}

async function bridgeRequest(path, token, fetchImpl = fetch) {
  try {
    return await fetchImpl(`${BRIDGE}${path}`, { headers: { Authorization: `Bearer ${token}` } });
  } catch (error) {
    if (isConnectionRefused(error)) throw new BridgeUnreachableError(error);
    throw error;
  }
}

export async function connectedAccounts(token, upstream, fetchImpl = fetch) {
  const response = await bridgeRequest('/v1/providers', token, fetchImpl);
  if (!response.ok) throw new Error(`Ojak 계정 연결이 로그인을 거절했어요 (${response.status}).`);
  const body = await response.json();
  const entry = Array.isArray(body?.providers) ? body.providers.find(item => item?.provider === upstream) : undefined;
  return typeof entry?.accounts === 'number' ? entry.accounts : 0;
}

export async function fetchBridgeModels(token, upstream, fetchImpl = fetch) {
  const response = await bridgeRequest('/v1/models', token, fetchImpl);
  if (!response.ok) throw new Error(`Ojak 모델 목록을 읽지 못했어요 (${response.status}).`);
  const body = await response.json();
  return bridgedModels(body?.data, upstream);
}

/// 브릿지 `/v1/models` 행 → 해당 Ojak 공급자의 모델. 원래 공급자 것만 고르고 `provider/` 접두어를 뗀다.
/// 비용·thinking 단계는 브릿지가 모르므로 비워 두고, 원래 공급자 행이 있으면 modifyModels가 채운다.
export function bridgedModels(rows, upstream) {
  const models = [];
  for (const row of Array.isArray(rows) ? rows : []) {
    if (row?.owned_by !== upstream || typeof row.id !== 'string') continue;
    const id = row.id.startsWith(`${upstream}/`) ? row.id.slice(upstream.length + 1) : row.id;
    if (!id) continue;
    const input = Array.isArray(row.input_modalities) && row.input_modalities.includes('image') ? ['text', 'image'] : ['text'];
    models.push({
      id,
      name: typeof row.display_name === 'string' && row.display_name ? row.display_name : id,
      reasoning: false,
      input,
      cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
      contextWindow: Number.isFinite(row.context_length) && row.context_length > 0 ? row.context_length : 128000,
      maxTokens: Number.isFinite(row.max_output_tokens) && row.max_output_tokens > 0 ? row.max_output_tokens : 16384,
      baseUrl: BRIDGE,
      api: ADMISSION_API,
    });
  }
  return models;
}

/// 원래 공급자 행이 있으면 그 정보를 쓰고(전송만 브릿지로), 없으면 브릿지에서 받은 Ojak 행을 그대로 둔다.
export function projectModels(models, name, upstream) {
  const upstreamRows = models.filter(model => model.provider === upstream);
  const upstreamIds = new Set(upstreamRows.map(model => model.id));
  const bridged = upstreamRows.map(({ transport: _transport, ...model }) => ({ ...model, provider: name, baseUrl: BRIDGE, api: ADMISSION_API }));
  const own = models.filter(model => model.provider === name && model.id !== 'ojak-login-required' && !upstreamIds.has(model.id))
    .map(({ transport: _transport, ...model }) => ({ ...model, baseUrl: BRIDGE, api: ADMISSION_API }));
  return [...models.filter(model => model.provider !== name), ...own, ...bridged];
}

/// 모델 호출이 로컬 브릿지 연결 거절로 실패하면 그 원인을 한 번 알린다. omp의 오류 표시·재시도는 그대로 두고
/// (알림은 `ctx.ui.notify`뿐), 계정·인증·사용량 오류는 건드리지 않는다. 프롬프트를 다시 보내거나 직접 인증 경로를 만들지 않는다.
/// 연결 거절 글자만으로 단정하지 않고 `/healthz` 프로브가 실제로 거절될 때만 브릿지 탓으로 알린다.
export function registerOutageHooks(pi, outage) {
  let closed = false;
  const report = ctx => {
    outage.failure(message => {
      if (closed) return;
      try { ctx.ui?.notify(message, 'error'); } catch {}
    }).catch(() => {});
  };
  const bridged = (message, ctx) => isBridgeProvider(message?.provider) || isBridgeProvider(ctx?.model?.provider);
  // 모델 호출 실패는 `stopReason: 'error'`인 assistant 메시지로 온다. 사용자가 취소하면 `aborted`라 여기서 다루지 않는다.
  pi.on('message_end', (event, ctx) => {
    const message = event?.message;
    if (message?.role !== 'assistant' || !bridged(message, ctx)) return;
    if (message.stopReason === 'error') {
      if (isConnectionRefused(message.errorMessage)) report(ctx);
    } else if (message.stopReason === 'stop' || message.stopReason === 'length' || message.stopReason === 'toolUse') {
      outage.recovered();
    }
  });
  // omp 자체 재시도 알림에도 같은 오류 문구가 실린다. 같은 중단이면 위에서 이미 알렸으므로 합쳐진다.
  pi.on('auto_retry_start', (event, ctx) => {
    if (isBridgeProvider(ctx?.model?.provider) && isConnectionRefused(event?.errorMessage)) report(ctx);
  });
  pi.on('auto_retry_end', (event, ctx) => {
    if (event?.success === true && isBridgeProvider(ctx?.model?.provider)) outage.recovered();
  });
  // 어떤 HTTP 응답이든 브릿지에 닿았다는 증거다(인증·할당량 오류 응답 포함).
  pi.on('after_provider_response', (_event, ctx) => {
    if (isBridgeProvider(ctx?.model?.provider)) outage.recovered();
  });
  pi.on('session_shutdown', () => { closed = true; });
}

export default async function aamAccounts(pi) {
  const ai = await import('@oh-my-pi/pi-ai');
  registerAdmissionHooks(pi);
  const outage = createOutageReporter();
  registerOutageHooks(pi, outage);
  for (const [name, upstream, label] of PROVIDERS) {
    const credentials = token => ({ access: token, refresh: token, expires: Date.now() + LIFETIME_MS });
    pi.registerProvider(name, {
      baseUrl: BRIDGE,
      api: ADMISSION_API,
      streamSimple: (model, context, options) => durableNativeStream(ai, model, context, options),
      // omp는 모델이 하나 이상 있는 공급자에만 modifyModels를 건다. 로그인 전 자리표시 모델이며,
      // 로그인하면 아래 modifyModels가 원래 공급자의 실제 모델로 바꾼다. 로그인 전에는 인증이 없어 선택 목록에 뜨지 않는다.
      models: [
        {
          id: 'ojak-login-required',
          name: `${label} (연결 확인 필요)`,
          reasoning: false,
          input: ['text'],
          cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
          contextWindow: 8192,
          maxTokens: 1024,
        },
      ],
      oauth: {
        name: label,
        async login() {
          const token = bridgeToken();
          const accounts = await connectedAccounts(token, upstream);
          outage.recovered();
          if (accounts === 0) {
            const existing = upstream === 'anthropic' || upstream === 'openai-codex'
              ? '기존 CLI에 로그인했다면 Ojak → 연결에서 해당 계정 상태를 확인해 주세요. omp에서 다시 로그인할 필요는 없어요.'
              : `omp /login ${upstream}에서 원래 공급자에 로그인해 주세요. Ojak에는 별도 로그인이 필요 없어요.`;
            throw new Error(`Ojak의 ${label.slice('Ojak · '.length)} 계정 연결이 아직 준비되지 않았어요. ${existing}`);
          }
          return credentials(token);
        },
        async refreshToken() {
          return credentials(bridgeToken());
        },
        getApiKey(stored) {
          return stored.access;
        },
        // 원래 공급자의 모델 정보(thinking 단계·비용·컨텍스트)가 있으면 그대로 쓰고, 전송만 브릿지로 바꾼다.
        modifyModels(models) {
          return projectModels(models, name, upstream);
        },
      },
      // 브릿지가 아는 실제 계정 모델. 토큰이 없거나 실패하면 예외를 던진다. 빈 목록을 돌려주면
      // 권위 있는 목록으로 받아들여 "로그인 필요" 자리표시까지 지워지므로, 실패는 이전 캐시·정적 목록을 유지하게 한다.
      async fetchDynamicModels() {
        const models = await fetchBridgeModels(bridgeToken(), upstream);
        outage.recovered();
        return models;
      },
    });
  }
}
