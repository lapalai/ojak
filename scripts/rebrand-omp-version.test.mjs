import { test } from 'node:test';
import assert from 'node:assert/strict';
import { REBRAND_FILES, RebrandError, rebrandText } from './rebrand-omp-version.mjs';

const names = ['pi-agent-core', 'pi-ai', 'pi-coding-agent', 'pi-utils'];
const catalog = version => `{\n  "catalog": {\n${[...names, 'pi-tui'].map(n => `    "@oh-my-pi/${n}": "${n === 'pi-tui' ? '9.9.9' : version}"`).join(',\n')}\n  }\n}\n`;
const lock = version => `${names.map(n => `    "packages/${n}": {\n      "name": "@oh-my-pi/${n}",\n      "version": "${version}",\n    },\n`).join('')}  "catalog": {\n${names.map(n => `    "@oh-my-pi/${n}": "${version}",\n`).join('')}    "@oh-my-pi/pi-tui": "9.9.9",\n  }\n`;

test('rewrites exactly the fork packages and leaves other workspace packages alone', () => {
  const out = rebrandText('package.json', catalog('1.2.3'), '1.2.3-ojak.1');
  assert.equal(out.match(/-ojak\.1/g).length, 4);
  assert.match(out, /"@oh-my-pi\/pi-tui": "9\.9\.9"/);
  const locked = rebrandText('bun.lock', lock('1.2.3'), '1.2.3-ojak.1');
  assert.equal(locked.match(/-ojak\.1/g).length, 8);
  assert.match(locked, /"@oh-my-pi\/pi-tui": "9\.9\.9"/);
});

test('is a pure, idempotent function of text and version, including going back to plain upstream', () => {
  const branded = rebrandText('bun.lock', lock('1.2.3'), '1.2.3-ojak.1');
  assert.equal(rebrandText('bun.lock', branded, '1.2.3-ojak.1'), branded);
  assert.equal(rebrandText('bun.lock', branded, '1.2.3'), lock('1.2.3'));
  assert.equal(rebrandText('bun.lock', lock('1.2.0'), '1.2.3-ojak.1'), branded);
});

test('package.json rewrite checks the package name and refuses ambiguity', () => {
  const text = '{\n  "name": "@oh-my-pi/pi-ai",\n  "version": "1.2.3"\n}\n';
  assert.match(rebrandText('packages/ai/package.json', text, '1.2.3-ojak.2'), /"version": "1\.2\.3-ojak\.2"/);
  assert.throws(() => rebrandText('packages/agent/package.json', text, '1.2.3-ojak.2'), RebrandError);
  assert.throws(() => rebrandText('packages/ai/package.json', '{ nope', '1.2.3-ojak.2'), RebrandError);
});

test('a file shape upstream changed fails closed instead of being rewritten partially', () => {
  assert.throws(() => rebrandText('package.json', catalog('1.2.3').replace('"@oh-my-pi/pi-ai"', '"@oh-my-pi/pi-renamed"'), '1.2.3-ojak.1'), /expected 4 version line\(s\), found 3/);
  assert.throws(() => rebrandText('bun.lock', lock('1.2.3').replace('"name": "@oh-my-pi/pi-ai",\n      "version"', '"name": "@oh-my-pi/pi-ai",\n      "private": true,\n      "version"'), '1.2.3-ojak.1'), RebrandError);
  assert.throws(() => rebrandText('CHANGELOG.md', '', '1.2.3-ojak.1'), /not a version-rebrand file/);
  for (const version of ['1.2', '1.2.3-ojak.0', 'v1.2.3', '1.2.3-canary.1']) assert.throws(() => rebrandText('package.json', catalog('1.2.3'), version), /Invalid version/);
  assert.equal(REBRAND_FILES.length, 6);
});
