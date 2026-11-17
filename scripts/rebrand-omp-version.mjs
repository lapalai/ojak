import { readFileSync, writeFileSync, existsSync } from 'node:fs';
import { join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

// The Ojak omp fork identifies itself by `X.Y.Z-ojak.N`. Upstream bumps the same lines on every release, so the
// patch must not carry them as ordinary hunks: they would conflict on each new upstream tag. The fork version is
// instead a pure function of (file text, version) over exactly these files, applied after the semantic patch.
const NAMES = '(?:pi-agent-core|pi-ai|pi-coding-agent|pi-utils)';
const VERSION = '\\d+\\.\\d+\\.\\d+(?:-ojak\\.[1-9]\\d*)?';
export const VERSION_PATTERN = new RegExp(`^${VERSION}$`);
const catalogLine = new RegExp(`("@oh-my-pi/${NAMES}"\\s*:\\s*")(${VERSION})(")`, 'g');
const lockWorkspace = new RegExp(`("name"\\s*:\\s*"@oh-my-pi/${NAMES}",\\s*"version"\\s*:\\s*")(${VERSION})(")`, 'g');
const packageNames = new Set(['pi-agent-core', 'pi-ai', 'pi-coding-agent', 'pi-utils']);
const packageDirs = { 'packages/agent/package.json': 'pi-agent-core', 'packages/ai/package.json': 'pi-ai', 'packages/coding-agent/package.json': 'pi-coding-agent', 'packages/utils/package.json': 'pi-utils' };

// Every file whose version lines belong to the fork rebrand, in a stable order.
export const REBRAND_FILES = ['package.json', ...Object.keys(packageDirs), 'bun.lock'];

function replaceExactly(text, pattern, version, expected, label) {
  let count = 0;
  const result = text.replace(pattern, (_, before, _old, after) => { count += 1; return `${before}${version}${after}`; });
  if (count !== expected) throw new RebrandError(`${label}: expected ${expected} version line(s), found ${count}`);
  return result;
}

export class RebrandError extends Error {}

/** Returns `text` of the repository-relative file `relative` with the fork packages' version set to `version`. */
export function rebrandText(relative, text, version) {
  if (!VERSION_PATTERN.test(version)) throw new RebrandError(`Invalid version: ${version}`);
  if (relative === 'package.json') return replaceExactly(text, catalogLine, version, 4, relative);
  if (relative === 'bun.lock') {
    return replaceExactly(replaceExactly(text, lockWorkspace, version, 4, `${relative} workspaces`), catalogLine, version, 4, `${relative} catalog`);
  }
  if (relative in packageDirs) {
    let parsed;
    try { parsed = JSON.parse(text); } catch { throw new RebrandError(`${relative}: not valid JSON`); }
    if (parsed.name !== `@oh-my-pi/${packageDirs[relative]}`) throw new RebrandError(`${relative}: unexpected package name ${parsed.name}`);
    return replaceExactly(text, new RegExp(`^(\\s*"version"\\s*:\\s*")(${VERSION})(")`, 'm'), version, 1, relative);
  }
  throw new RebrandError(`${relative} is not a version-rebrand file`);
}

/** Rewrites every rebrand file under `directory` in place. Writes nothing unless all six files rewrite cleanly. */
export function rebrandDirectory(directory, version) {
  const root = resolve(directory);
  const rewritten = REBRAND_FILES.map(relative => {
    const file = join(root, relative);
    if (!existsSync(file)) throw new RebrandError(`${relative}: missing`);
    return [file, rebrandText(relative, readFileSync(file, 'utf8'), version)];
  });
  for (const [file, text] of rewritten) writeFileSync(file, text);
  return REBRAND_FILES;
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  const [directory, version] = process.argv.slice(2);
  if (!directory || !version || process.argv.length !== 4) {
    console.error('Usage: node scripts/rebrand-omp-version.mjs <oh-my-pi-checkout> <X.Y.Z-ojak.N>');
    process.exit(2);
  }
  try {
    console.log(`Rebranded ${rebrandDirectory(directory, version).join(', ')} to ${version}.`);
  } catch (error) {
    if (!(error instanceof RebrandError)) throw error;
    console.error(error.message);
    process.exit(10);
  }
}
