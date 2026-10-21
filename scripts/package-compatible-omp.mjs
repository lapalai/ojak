import { createHash } from 'node:crypto';
import { createReadStream } from 'node:fs';
import { copyFile, mkdir, readFile, stat, writeFile } from 'node:fs/promises';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { spawnSync } from 'node:child_process';

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const platforms = { 'darwin-arm64': 'omp-darwin-arm64', 'win32-x64': 'omp-windows-x64.exe' };

export async function packageCompatibleOmp(source, destination, platform = `${process.platform}-${process.arch}`, run = spawnSync) {
  const asset = platforms[platform];
  if (!asset || platform !== `${process.platform}-${process.arch}`) throw new Error('Build and verify each omp asset on its native supported host');
  const pins = JSON.parse(await readFile(join(root, 'integrations/client-forks/manifest.json'), 'utf8'));
  const expected = pins.clients.find(client => client.directory === 'oh-my-pi').version;
  const packageVersion = JSON.parse(await readFile(join(source, 'packages/coding-agent/package.json'), 'utf8')).version;
  if (packageVersion !== expected) throw new Error('Prepared omp version does not match pinned fork');
  const runChecked = (command, args, cwd, capture = false) => {
    const result = run(command, args, { cwd, encoding: 'utf8', stdio: capture ? ['ignore', 'pipe', 'pipe'] : 'inherit', timeout: capture ? 30_000 : 90 * 60_000, windowsHide: true });
    if (result.error || result.status !== 0) throw new Error(`${command} failed: ${result.error?.message ?? result.status}`);
    return result.stdout ?? '';
  };
  runChecked('bun', ['install', '--frozen-lockfile'], source);
  runChecked('bun', ['run', 'build:native'], source);
  runChecked('bun', ['scripts/ci-release-build-binaries.ts', '--targets', platform], source);
  const output = join(source, 'packages/coding-agent/binaries', asset);
  const reported = runChecked(output, ['--version'], source, true).trim();
  if (!reported.split(/\s+/).some(word => word === expected || word === `omp/${expected}`)) throw new Error(`Built omp did not report ${expected}`);
  await mkdir(destination, { recursive: true });
  await copyFile(output, join(destination, asset));
  console.log(`Packaged ${asset} (${expected}); not installed or published.`);
}

export async function generateCompatibleManifest(directory, version) {
  if (!/^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)-ojak\.([1-9]\d*)$/.test(version)) throw new Error('Invalid compatible fork version');
  const entries = {};
  for (const [platform, asset] of Object.entries(platforms)) {
    const file = join(directory, asset);
    const info = await stat(file);
    if (!info.isFile() || info.size <= 0 || info.size > 1024 ** 3) throw new Error(`Invalid compatible binary: ${asset}`);
    const hash = createHash('sha256');
    for await (const chunk of createReadStream(file)) hash.update(chunk);
    entries[platform] = { asset, sha256: hash.digest('hex'), size: info.size };
  }
  const manifest = { schemaVersion: 1, admissionProtocol: 1, version, platforms: entries };
  await writeFile(join(directory, 'omp-compatible.json'), `${JSON.stringify(manifest, null, 2)}\n`);
  return manifest;
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  const [mode, source, destination] = process.argv.slice(2);
  if (mode === 'build' && source && destination && process.argv.length === 5) {
    await packageCompatibleOmp(resolve(source), resolve(destination));
  } else if (mode === 'manifest' && source && !destination && process.argv.length === 4) {
    const pins = JSON.parse(await readFile(join(root, 'integrations/client-forks/manifest.json'), 'utf8'));
    await generateCompatibleManifest(resolve(source), pins.clients.find(client => client.directory === 'oh-my-pi').version);
  } else {
    throw new Error('Usage: node scripts/package-compatible-omp.mjs build <prepared-source> <stage> | manifest <stage>');
  }
}
