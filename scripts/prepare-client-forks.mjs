import { createHash } from 'node:crypto';
import { existsSync, mkdirSync, readFileSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { spawnSync } from 'node:child_process';

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const directory = join(root, 'integrations', 'client-forks');
const manifest = JSON.parse(readFileSync(join(directory, 'manifest.json'), 'utf8'));
const destination = process.argv[2];
if (!destination || process.argv.length !== 3) {
  throw new Error('Usage: node scripts/prepare-client-forks.mjs <empty-destination-directory|--check>');
}
function git(args, cwd, capture = false) {
  const result = spawnSync('git', args, { cwd, encoding: 'utf8', stdio: capture ? ['ignore', 'pipe', 'inherit'] : 'inherit', windowsHide: true });
  if (result.error) throw result.error;
  if (result.status !== 0) throw new Error(`git ${args[0]} failed (${result.status})`);
  return result.stdout?.trim();
}
if (manifest.version !== 1 || !Array.isArray(manifest.clients) || manifest.clients.length !== 2) {
  throw new Error('Unsupported client fork manifest');
}
for (const client of manifest.clients) {
  if (!['oh-my-pi', 'orca'].includes(client.directory) || !/^[a-f0-9]{40}$/.test(client.revision) || !/^[a-f0-9]{64}$/.test(client.sha256)) {
    throw new Error('Invalid pinned client identity');
  }
  const patch = join(directory, `${client.directory}.patch`);
  const digest = createHash('sha256').update(readFileSync(patch)).digest('hex');
  if (digest !== client.sha256) throw new Error(`Patch checksum mismatch: ${client.directory}`);
  if (destination === '--check') continue;
  const target = resolve(destination, client.directory);
  if (existsSync(target)) throw new Error(`Refusing to overwrite an existing checkout: ${target}`);
}
if (destination === '--check') {
  console.log('Both pinned client patch checksums match. No checkouts or installations changed.');
} else {
  const targetRoot = resolve(destination);
  mkdirSync(targetRoot, { recursive: true });
  for (const client of manifest.clients) {
    const target = join(targetRoot, client.directory);
    git(['clone', '--depth', '1', '--branch', client.tag, '--', client.repository, target], root);
    if (git(['rev-parse', 'HEAD'], target, true) !== client.revision) {
      throw new Error(`Upstream tag moved: ${client.directory}. Checkout retained for inspection; patch not applied.`);
    }
    const patch = join(directory, `${client.directory}.patch`);
    git(['apply', '--check', '--', patch], target);
    git(['apply', '--', patch], target);
    console.log(`Prepared ${client.directory} at pinned base ${client.revision}. Not built or installed.`);
  }
}
