import assert from 'node:assert/strict';
import test from 'node:test';
import { bridgedModels, projectModels } from './aam-accounts.js';

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
  assert.equal(models[0].transport, 'pi-native');
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
