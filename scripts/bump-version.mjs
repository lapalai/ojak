import { readFileSync, writeFileSync } from 'node:fs';
import { resolve, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

/// `x.y.z` only. Pre-release and build metadata are refused so a tag `vX.Y.Z` can match the files exactly.
const SEMVER = /^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)$/;

const version = process.argv[2];
if (!version || !SEMVER.test(version)) {
  console.error('usage: node scripts/bump-version.mjs <x.y.z>');
  console.error('refusing a value that is not strict numeric semver (no v prefix, pre-release, or build metadata)');
  process.exit(1);
}

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..');

function replaceOnce(label, text, pattern) {
  let count = 0;
  const next = text.replace(pattern, (match, prefix, suffix) => {
    count += 1;
    return `${prefix}${version}${suffix}`;
  });
  if (count !== 1) {
    console.error(`${label}: expected exactly one version field, found ${count}`);
    process.exit(1);
  }
  return next;
}

function writeIfChanged(path, before, after) {
  if (before === after) {
    console.error(`${path}: version was already ${version}, but the rewrite matched nothing new`);
    process.exit(1);
  }
  writeFileSync(path, after);
}

const tauriPath = resolve(root, 'apps/desktop/src-tauri/tauri.conf.json');
const tauri = readFileSync(tauriPath, 'utf8');
const tauriNext = replaceOnce('tauri.conf.json', tauri, /("version"\s*:\s*")[^"]+(")/);
JSON.parse(tauriNext);
writeIfChanged(tauriPath, tauri, tauriNext);

const cargoPath = resolve(root, 'Cargo.toml');
const cargo = readFileSync(cargoPath, 'utf8');
const cargoNext = replaceOnce(
  'Cargo.toml [workspace.package]',
  cargo,
  /(\[workspace\.package\][^\[]*?^version = ")[^"]+(")/m,
);
writeIfChanged(cargoPath, cargo, cargoNext);

for (const rel of ['package.json', 'apps/desktop/package.json']) {
  const path = resolve(root, rel);
  const text = readFileSync(path, 'utf8');
  const next = replaceOnce(rel, text, /("version"\s*:\s*")[^"]+(")/);
  const parsed = JSON.parse(next);
  if (parsed.version !== version) {
    console.error(`${rel}: parsed version is ${parsed.version}, expected ${version}`);
    process.exit(1);
  }
  writeIfChanged(path, text, next);
}

console.log(`version ${version}`);
