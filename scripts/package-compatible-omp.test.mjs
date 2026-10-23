import { afterEach, test } from 'node:test';
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { mkdtemp, readFile, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { generateCompatibleManifest } from './package-compatible-omp.mjs';

const directories = [];
afterEach(async () => { await Promise.all(directories.splice(0).map(path => rm(path, { recursive: true, force: true }))); });
async function fixture() {
  const directory = await mkdtemp(join(tmpdir(), 'ojak-compatible-package-'));
  directories.push(directory);
  await writeFile(join(directory, 'omp-darwin-arm64'), Buffer.from([0xcf, 0xfa, 0xed, 0xfe, 1]));
  await writeFile(join(directory, 'omp-windows-x64.exe'), Buffer.from([0x4d, 0x5a, 2, 3, 4, 5]));
  return directory;
}
test('manifest binds both exact asset byte streams and the compatible version', async () => {
  const directory = await fixture();
  const result = await generateCompatibleManifest(directory, '18.12.0-ojak.2');
  assert.deepEqual(JSON.parse(await readFile(join(directory, 'omp-compatible.json'), 'utf8')), result);
  assert.equal(result.schemaVersion, 1);
  assert.equal(result.admissionProtocol, 1);
  assert.equal(result.version, '18.12.0-ojak.2');
  for (const [platform, asset] of [['darwin-arm64', 'omp-darwin-arm64'], ['win32-x64', 'omp-windows-x64.exe']]) {
    const bytes = await readFile(join(directory, asset));
    assert.deepEqual(result.platforms[platform], { asset, size: bytes.length, sha256: createHash('sha256').update(bytes).digest('hex') });
  }
});
test('missing Windows build refuses the manifest instead of producing a partial release', async () => {
  const directory = await fixture();
  await rm(join(directory, 'omp-windows-x64.exe'));
  await assert.rejects(generateCompatibleManifest(directory, '18.12.0-ojak.2'), /ENOENT/);
  await assert.rejects(readFile(join(directory, 'omp-compatible.json')), /ENOENT/);
});
test('empty binary and upstream or unrelated prerelease versions cannot be packaged', async () => {
  const directory = await fixture();
  for (const version of ['18.12.0', '18.12.0-canary.1', '18.12.0-ojak.0', '018.12.0-ojak.2']) {
    await assert.rejects(generateCompatibleManifest(directory, version), /Invalid compatible fork version/);
  }
  await writeFile(join(directory, 'omp-darwin-arm64'), '');
  await assert.rejects(generateCompatibleManifest(directory, '18.12.0-ojak.2'), /Invalid compatible binary/);
  await assert.rejects(readFile(join(directory, 'omp-compatible.json')), /ENOENT/);
});
