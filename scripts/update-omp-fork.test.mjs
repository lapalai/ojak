import { afterEach, test } from 'node:test';
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { copyFileSync, existsSync, mkdirSync, mkdtempSync, readFileSync, readdirSync, rmSync, writeFileSync } from 'node:fs';
import { devNull, tmpdir } from 'node:os';
import { dirname, join } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { spawnSync } from 'node:child_process';
import { checkCandidate, compareTags, renderIssue, updateOmpFork } from './update-omp-fork.mjs';
import { rebrandDirectory } from './rebrand-omp-version.mjs';

const scripts = dirname(fileURLToPath(import.meta.url));
const env = { ...process.env, GIT_AUTHOR_NAME: 't', GIT_AUTHOR_EMAIL: 't@invalid', GIT_COMMITTER_NAME: 't', GIT_COMMITTER_EMAIL: 't@invalid', GIT_CONFIG_GLOBAL: devNull, GIT_CONFIG_NOSYSTEM: '1' };
const directories = [];
afterEach(() => { for (const directory of directories.splice(0)) rmSync(directory, { recursive: true, force: true }); });

function git(cwd, ...args) {
  const result = spawnSync('git', ['-c', 'core.autocrlf=false', ...args], { cwd, env, encoding: 'utf8' });
  assert.equal(result.status, 0, `git ${args.join(' ')}: ${result.stderr}`);
  return result.stdout.trim();
}
const sha = bytes => createHash('sha256').update(bytes).digest('hex');
const names = ['pi-agent-core', 'pi-ai', 'pi-coding-agent', 'pi-utils'];
const dirs = { 'pi-agent-core': 'agent', 'pi-ai': 'ai', 'pi-coding-agent': 'coding-agent', 'pi-utils': 'utils' };

function write(root, relative, text) { mkdirSync(dirname(join(root, relative)), { recursive: true }); writeFileSync(join(root, relative), text); }
function upstreamFiles(root, version, extra = {}) {
  write(root, 'package.json', `{\n  "catalog": {\n${names.map(n => `    "@oh-my-pi/${n}": "${version}",\n`).join('')}    "@oh-my-pi/pi-tui": "${version}"\n  }\n}\n`);
  for (const n of names) write(root, `packages/${dirs[n]}/package.json`, `{\n  "name": "@oh-my-pi/${n}",\n  "version": "${version}"\n}\n`);
  write(root, 'bun.lock', `{\n${names.map(n => `    "packages/${dirs[n]}": {\n      "name": "@oh-my-pi/${n}",\n      "version": "${version}",\n    },\n`).join('')}  "catalog": {\n${names.map(n => `    "@oh-my-pi/${n}": "${version}",\n`).join('')}    "@oh-my-pi/pi-tui": "${version}",\n  }\n}\n`);
  for (const [file, text] of Object.entries(extra)) write(root, file, text);
}
const feature = ['export function admit() {', '  return "upstream";', '}', ''];
const other = Array.from({ length: 30 }, (_, i) => `line ${i}`).concat('');

/**
 * Builds an upstream repo with v1.0.0 (lightweight), v1.1.0 (annotated, unrelated change), v1.2.0 (changes the lines
 * the fork patch edits) and a fork patch made on v1.0.0, plus a fixture Ojak root with the pins and a git history.
 */
function fixture() {
  const base = mkdtempSync(join(tmpdir(), 'ojak-omp-update-test-'));
  directories.push(base);
  const upstream = join(base, 'upstream');
  mkdirSync(upstream);
  git(upstream, 'init', '-q', '-b', 'main');
  upstreamFiles(upstream, '1.0.0', { 'src/feature.ts': feature.join('\n'), 'src/other.ts': other.join('\n') });
  git(upstream, 'add', '-A'); git(upstream, 'commit', '-q', '-m', 'v1.0.0'); git(upstream, 'tag', 'v1.0.0');
  const r100 = git(upstream, 'rev-parse', 'HEAD');

  // v1.1.0: version bump + unrelated edit far from the fork's lines.
  upstreamFiles(upstream, '1.1.0', { 'src/other.ts': other.map(l => (l === 'line 28' ? 'line 28 changed upstream' : l)).join('\n') });
  git(upstream, 'add', '-A'); git(upstream, 'commit', '-q', '-m', 'v1.1.0'); git(upstream, 'tag', '-a', 'v1.1.0', '-m', 'v1.1.0');
  const r110 = git(upstream, 'rev-parse', 'HEAD');

  // v1.2.0 (from v1.0.0 line, conflicting): rewrites the line the fork patch also rewrites.
  git(upstream, 'checkout', '-q', '-b', 'conflict', r110);
  upstreamFiles(upstream, '1.2.0', { 'src/feature.ts': feature.map(l => (l.includes('upstream') ? '  return "upstream rewrote this";' : l)).join('\n') });
  git(upstream, 'add', '-A'); git(upstream, 'commit', '-q', '-m', 'v1.2.0'); git(upstream, 'tag', 'v1.2.0');

  // The fork patch, made exactly the way the real one is: semantic edits + fork version on the six files.
  const work = join(base, 'fork-work');
  git(base, 'clone', '-q', '--branch', 'v1.0.0', pathToFileURL(upstream).href, work);
  write(work, 'src/feature.ts', feature.map(l => (l.includes('upstream') ? '  return "ojak admission";' : l)).join('\n'));
  write(work, 'src/ojak.ts', 'export const ojak = true;\n');
  rebrandDirectory(work, '1.0.0-ojak.1');
  git(work, 'add', '-A');
  const patch = spawnSync('git', ['diff', '--cached', '--binary', '--full-index', '--no-renames'], { cwd: work, env }).stdout;

  // Orca stand-in so the real prepare-client-forks.mjs accepts the manifest.
  const orca = join(base, 'orca');
  mkdirSync(orca); git(orca, 'init', '-q', '-b', 'main'); write(orca, 'a.txt', 'a\n'); git(orca, 'add', '-A'); git(orca, 'commit', '-q', '-m', 'o'); git(orca, 'tag', 'v1.0.0');
  const orcaRevision = git(orca, 'rev-parse', 'HEAD');
  const orcaPatch = 'diff --git a/a.txt b/a.txt\n--- a/a.txt\n+++ b/a.txt\n@@ -1 +1,2 @@\n a\n+fork\n';

  const root = join(base, 'root');
  mkdirSync(join(root, 'integrations/client-forks'), { recursive: true });
  mkdirSync(join(root, 'scripts'));
  copyFileSync(join(scripts, 'prepare-client-forks.mjs'), join(root, 'scripts/prepare-client-forks.mjs'));
  writeFileSync(join(root, 'integrations/client-forks/oh-my-pi.patch'), patch);
  writeFileSync(join(root, 'integrations/client-forks/orca.patch'), orcaPatch);
  const manifest = {
    version: 1,
    clients: [
      { directory: 'oh-my-pi', repository: pathToFileURL(upstream).href, tag: 'v1.0.0', revision: r100, version: '1.0.0-ojak.1', admissionContract: 1, sha256: sha(patch) },
      { directory: 'orca', repository: pathToFileURL(orca).href, tag: 'v1.0.0', revision: orcaRevision, version: '1.0.0-ojak.1', sha256: sha(orcaPatch) },
    ],
  };
  writeFileSync(join(root, 'integrations/client-forks/manifest.json'), `${JSON.stringify(manifest, null, 2)}\n`);
  git(root, 'init', '-q', '-b', 'main'); git(root, 'add', '-A'); git(root, 'commit', '-q', '-m', 'pin');
  return { base, upstream, root, r100, r110, patchPath: join(root, 'integrations/client-forks/oh-my-pi.patch'), manifestPath: join(root, 'integrations/client-forks/manifest.json') };
}
const pin = f => JSON.parse(readFileSync(f.manifestPath, 'utf8')).clients[0];
const snapshot = f => ({ patch: readFileSync(f.patchPath), manifest: readFileSync(f.manifestPath) });
function assertUntouched(f, before) {
  assert.deepEqual(snapshot(f), before);
  assert.deepEqual(readdirSync(join(f.root, 'integrations/client-forks')).sort(), ['manifest.json', 'oh-my-pi.patch', 'orca.patch']);
}

test('clean bump produces a valid, prepare-compatible patch and manifest for the newer tag', () => {
  const f = fixture();
  const report = updateOmpFork({ root: f.root, tag: 'v1.1.0', expectRevision: f.r110 });
  assert.equal(report.status, 'updated', JSON.stringify(report));
  assert.equal(report.candidateVersion, '1.1.0-ojak.1');
  const entry = pin(f);
  assert.deepEqual({ tag: entry.tag, revision: entry.revision, version: entry.version, admissionContract: entry.admissionContract }, { tag: 'v1.1.0', revision: f.r110, version: '1.1.0-ojak.1', admissionContract: 1 });
  assert.equal(entry.sha256, sha(readFileSync(f.patchPath)));
  // Only the oh-my-pi entry moved.
  assert.equal(JSON.parse(readFileSync(f.manifestPath, 'utf8')).clients[1].tag, 'v1.0.0');

  // The unmodified, real prepare script accepts it and produces the fork on the new tag.
  const out = join(f.base, 'prepared');
  const prepared = spawnSync(process.execPath, [join(f.root, 'scripts/prepare-client-forks.mjs'), out], { env, encoding: 'utf8' });
  assert.equal(prepared.status, 0, prepared.stderr + prepared.stdout);
  const source = join(out, 'oh-my-pi');
  assert.equal(JSON.parse(readFileSync(join(source, 'packages/coding-agent/package.json'), 'utf8')).version, '1.1.0-ojak.1');
  assert.match(readFileSync(join(source, 'bun.lock'), 'utf8'), /"version": "1\.1\.0-ojak\.1"/);
  assert.match(readFileSync(join(source, 'package.json'), 'utf8'), /"@oh-my-pi\/pi-tui": "1\.1\.0"/);
  assert.match(readFileSync(join(source, 'src/feature.ts'), 'utf8'), /ojak admission/);
  assert.match(readFileSync(join(source, 'src/other.ts'), 'utf8'), /line 28 changed upstream/);
  assert.equal(readFileSync(join(source, 'src/ojak.ts'), 'utf8'), 'export const ojak = true;\n');
  // Version-only hunks are confined to the six rebrand files.
  const files = git(source, 'diff', '--name-only').split('\n').sort();
  assert.deepEqual(files, ['bun.lock', 'package.json', 'packages/agent/package.json', 'packages/ai/package.json', 'packages/coding-agent/package.json', 'packages/utils/package.json', 'src/feature.ts'].sort());
  assert.equal(spawnSync(process.execPath, [join(f.root, 'scripts/prepare-client-forks.mjs'), '--check'], { env }).status, 0);
});

test('the same inputs always produce byte-identical patch and manifest, and re-running on the new pin changes nothing', () => {
  const a = fixture(); const b = fixture();
  updateOmpFork({ root: a.root, tag: 'v1.1.0' });
  updateOmpFork({ root: b.root, tag: 'v1.1.0' });
  assert.deepEqual(readFileSync(a.patchPath), readFileSync(b.patchPath));
  const before = snapshot(a);
  const again = updateOmpFork({ root: a.root, tag: 'v1.1.0' });
  assert.equal(again.status, 'up-to-date');
  assertUntouched(a, before);
});

test('same version with the same revision is a no-op and an older tag never downgrades', () => {
  const f = fixture();
  const before = snapshot(f);
  assert.equal(updateOmpFork({ root: f.root, tag: 'v1.0.0' }).status, 'up-to-date');
  assertUntouched(f, before);
  updateOmpFork({ root: f.root, tag: 'v1.1.0' });
  const after = snapshot(f);
  assert.equal(updateOmpFork({ root: f.root, tag: 'v1.0.0' }).status, 'not-newer');
  assertUntouched(f, after);
});

test('a real conflict is reported with its files and leaves the pinned release exactly as it was', () => {
  const f = fixture();
  const before = snapshot(f);
  const report = updateOmpFork({ root: f.root, tag: 'v1.2.0' });
  assert.equal(report.status, 'conflict');
  assert.deepEqual(report.conflicts, ['src/feature.ts']);
  assert.match(report.message, /does not merge cleanly/);
  assertUntouched(f, before);
  const issue = renderIssue(report);
  assert.match(issue.title, /v1\.2\.0 conflicts/);
  assert.match(issue.body, /src\/feature\.ts/);
  assert.match(issue.body, /Nothing was changed/);
  assert.match(issue.body, /not\*\* the upstream latest/);
  // CLI contract: conflict is exit 10 with a report file.
  const reportFile = join(f.base, 'report.json');
  const cli = spawnSync(process.execPath, [join(scripts, 'update-omp-fork.mjs'), '--root', f.root, '--tag', 'v1.2.0', '--report', reportFile], { env, encoding: 'utf8' });
  assert.equal(cli.status, 10, cli.stderr);
  assert.equal(JSON.parse(readFileSync(reportFile, 'utf8')).status, 'conflict');
  assertUntouched(f, before);
});

test('a moved or unexpected tag is refused rather than followed', () => {
  const f = fixture();
  const before = snapshot(f);
  const wrong = updateOmpFork({ root: f.root, tag: 'v1.1.0', expectRevision: 'a'.repeat(40) });
  assert.equal(wrong.status, 'refused');
  assert.match(wrong.message, /moved tag|expected/);
  assertUntouched(f, before);
  assert.equal(updateOmpFork({ root: f.root, tag: 'v1.1.0', expectRevision: 'nope' }).status, 'refused');

  // The pinned tag itself is force-moved upstream: the pin must not be silently re-trusted.
  git(f.upstream, 'checkout', '-q', 'main');
  write(f.upstream, 'src/other.ts', 'moved\n'); git(f.upstream, 'add', '-A'); git(f.upstream, 'commit', '-q', '-m', 'moved');
  git(f.upstream, 'tag', '-f', 'v1.0.0');
  const moved = updateOmpFork({ root: f.root, tag: 'v1.0.0' });
  assert.equal(moved.status, 'refused');
  assert.match(moved.message, /moved/);
  assertUntouched(f, before);
  // The moved pinned tag is never re-trusted, but the pinned *revision* is still the patch base, so a newer tag can proceed.
  assert.equal(updateOmpFork({ root: f.root, tag: 'v1.1.0', expectRevision: f.r110 }).status, 'updated');
});

test('a tampered patch, an inconsistent pin, and a non-stable or unknown tag are refused before anything is written', () => {
  const f = fixture();
  const before = snapshot(f);
  for (const tag of ['v1.1.0-rc.1', 'v1.1', '1.1.0', 'v9.9.9']) assert.equal(updateOmpFork({ root: f.root, tag }).status, 'refused', tag);
  assertUntouched(f, before);

  writeFileSync(f.patchPath, `${readFileSync(f.patchPath, 'utf8')}\n`);
  const tampered = updateOmpFork({ root: f.root, tag: 'v1.1.0' });
  assert.equal(tampered.status, 'refused');
  assert.match(tampered.message, /checksum mismatch/);
  writeFileSync(f.patchPath, before.patch);

  const manifest = JSON.parse(before.manifest);
  manifest.clients[0].version = '1.0.0';
  writeFileSync(f.manifestPath, JSON.stringify(manifest));
  assert.throws(() => updateOmpFork({ root: f.root, tag: 'v1.1.0' }), /valid pinned oh-my-pi fork/);
});

test('a manifest whose pinned revision is not the patch base is refused with no candidate', () => {
  const f = fixture();
  const manifest = JSON.parse(readFileSync(f.manifestPath, 'utf8'));
  manifest.clients[0].revision = f.r110; // patch was made on v1.0.0, but the pin claims v1.1.0's commit
  writeFileSync(f.manifestPath, `${JSON.stringify(manifest, null, 2)}\n`);
  const before = snapshot(f);
  const report = updateOmpFork({ root: f.root, tag: 'v1.2.0' });
  assert.ok(['refused', 'conflict'].includes(report.status), report.status);
  assertUntouched(f, before);
});

test('checkCandidate accepts exactly a pin-only newer candidate and nothing else', () => {
  const f = fixture();
  assert.throws(() => checkCandidate(f.root), /no candidate/);
  updateOmpFork({ root: f.root, tag: 'v1.1.0' });
  assert.deepEqual(checkCandidate(f.root), { tag: 'v1.1.0', revision: f.r110, version: '1.1.0-ojak.1' });
  const clean = snapshot(f);

  write(f.root, 'scripts/extra.mjs', 'x');
  assert.throws(() => checkCandidate(f.root), /outside the fork pin/);
  rmSync(join(f.root, 'scripts/extra.mjs'));

  const manifest = JSON.parse(clean.manifest);
  manifest.clients[1].tag = 'v9.9.9';
  writeFileSync(f.manifestPath, JSON.stringify(manifest));
  assert.throws(() => checkCandidate(f.root), /other than oh-my-pi/);

  const forged = JSON.parse(clean.manifest);
  forged.clients[0].sha256 = 'f'.repeat(64);
  writeFileSync(f.manifestPath, JSON.stringify(forged));
  assert.throws(() => checkCandidate(f.root), /sha256 does not match/);

  const downgraded = JSON.parse(clean.manifest);
  downgraded.clients[0].tag = 'v1.0.0';
  writeFileSync(f.manifestPath, JSON.stringify(downgraded));
  assert.throws(() => checkCandidate(f.root), /not newer|must be/);

  const changedContract = JSON.parse(clean.manifest);
  changedContract.clients[0].admissionContract = 2;
  writeFileSync(f.manifestPath, JSON.stringify(changedContract));
  assert.throws(() => checkCandidate(f.root), /admissionContract/);
  writeFileSync(f.manifestPath, clean.manifest);
  assert.ok(existsSync(f.patchPath));
});

test('stable tag ordering is numeric and rejects non-stable tags', () => {
  assert.equal(compareTags('v18.18.9', 'v18.15.0'), 1);
  assert.equal(compareTags('v18.9.0', 'v18.15.0'), -1);
  assert.equal(compareTags('v1.0.0', 'v1.0.0'), 0);
  assert.throws(() => compareTags('v1.0.0-rc.1', 'v1.0.0'), /stable/);
});
