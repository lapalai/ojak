// Ojak(오작) — omp `/login`에 Ojak 공급자를 추가한다. (OMP v18.3 ExtensionAPI)
//
// `/login` → "Ojak · Claude" 등을 고르면 Ojak 계정 브릿지 토큰을 로그인 정보로 저장한다.
// 모델 목록은 두 겹이다. ① `fetchDynamicModels`가 Ojak 브릿지에서 실제 계정의 모델을 직접 받는다(캐시됨).
// 원래 공급자의 목록이 늦게 와도 Ojak 모델이 비지 않는다. ② `modifyModels`는 원래 공급자 행이 있으면
// 그 정보(thinking 단계·비용)로 덮어쓴다. 요청은 pi-native 형식으로 Ojak 브릿지에 보내고, 계정은 Ojak이 고른다.
import { readFileSync } from 'node:fs';
import { homedir } from 'node:os';
import { join } from 'node:path';

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

async function connectedAccounts(token, upstream) {
  let response;
  try {
    response = await fetch(`${BRIDGE}/v1/providers`, { headers: { Authorization: `Bearer ${token}` } });
  } catch {
    throw new Error('Ojak 계정 연결에 닿지 못했어요. Ojak 서비스가 실행 중인지 확인해 주세요.');
  }
  if (!response.ok) throw new Error(`Ojak 계정 연결이 로그인을 거절했어요 (${response.status}).`);
  const body = await response.json();
  const entry = Array.isArray(body?.providers) ? body.providers.find(item => item?.provider === upstream) : undefined;
  return typeof entry?.accounts === 'number' ? entry.accounts : 0;
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
      transport: 'pi-native',
    });
  }
  return models;
}

/// 원래 공급자 행이 있으면 그 정보를 쓰고(전송만 브릿지로), 없으면 브릿지에서 받은 Ojak 행을 그대로 둔다.
export function projectModels(models, name, upstream) {
  const upstreamRows = models.filter(model => model.provider === upstream);
  const upstreamIds = new Set(upstreamRows.map(model => model.id));
  const bridged = upstreamRows.map(model => ({ ...model, provider: name, baseUrl: BRIDGE, transport: 'pi-native' }));
  const own = models.filter(model => model.provider === name && model.id !== 'ojak-login-required' && !upstreamIds.has(model.id));
  return [...models.filter(model => model.provider !== name), ...own, ...bridged];
}

export default function aamAccounts(pi) {
  for (const [name, upstream, label] of PROVIDERS) {
    const credentials = token => ({ access: token, refresh: token, expires: Date.now() + LIFETIME_MS });
    pi.registerProvider(name, {
      baseUrl: BRIDGE,
      api: 'openai-completions',
      // omp는 모델이 하나 이상 있는 공급자에만 modifyModels를 건다. 로그인 전 자리표시 모델이며,
      // 로그인하면 아래 modifyModels가 원래 공급자의 실제 모델로 바꾼다. 로그인 전에는 인증이 없어 선택 목록에 뜨지 않는다.
      models: [
        {
          id: 'ojak-login-required',
          name: `${label} (로그인 필요)`,
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
          if ((await connectedAccounts(token, upstream)) === 0) {
            throw new Error(`Ojak에 연결된 ${label.slice('Ojak · '.length)} 계정이 없어요. omp에서 원래 공급자로 /login하면 최대 1분 뒤에 보여요.`);
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
        const token = bridgeToken();
        const response = await fetch(`${BRIDGE}/v1/models`, { headers: { Authorization: `Bearer ${token}` } });
        if (!response.ok) throw new Error(`Ojak 모델 목록을 읽지 못했어요 (${response.status}).`);
        const body = await response.json();
        return bridgedModels(body?.data, upstream);
      },
    });
  }
}
