import { createHash } from 'node:crypto';
import { mkdtempSync, readFileSync, renameSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir, devNull } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { spawnSync } from 'node:child_process';
import { REBRAND_FILES, RebrandError, rebrandDirectory } from './rebrand-omp-version.mjs';

// Prepares a candidate Ojak omp fork for a newer upstream stable tag, or refuses without touching the pinned fork.
//
//   node scripts/update-omp-fork.mjs --tag vX.Y.Z [--expect-revision <sha>] [--report <file>] [--root <dir>]
//   node scripts/update-omp-fork.mjs --check-candidate [--base <rev, default HEAD>] [--root <git checkout>]
//   node scripts/update-omp-fork.mjs --render-issue <report.json> [--branch <name>] [--pr-failed]
//
// It only reads upstream through git (clone, apply, 3-way cherry-pick); it never installs or runs upstream code, so it
// is safe to run in a job that has no write token. On success it rewrites exactly oh-my-pi.patch and the oh-my-pi entry
// of manifest.json (tag, revision, version, sha256); the checksum pin that prepare-client-forks.mjs enforces stays the
// only trust anchor. Exit codes: 0 updated / up-to-date / not-newer, 10 conflict, 11 refused (trust boundary), 1 error.

const defaultRoot = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const CLIENT = 'oh-my-pi';
const TAG = /^v(\d+)\.(\d+)\.(\d+)$/;
const SHA = /^[a-f0-9]{40}$/;

export class Conflict extends Error {
  constructor(message, files = [], output = '') { super(message); this.files = files; this.output = output; }
}
export class Refusal extends Error {}

const sha256 = bytes => createHash('sha256').update(bytes).digest('hex');
const parseTag = tag => { const m = TAG.exec(tag ?? ''); return m ? m.slice(1).map(Number) : null; };
export function compareTags(a, b) {
  const x = parseTag(a); const y = parseTag(b);
  if (!x || !y) throw new Refusal(`Not a stable vX.Y.Z tag: ${!x ? a : b}`);
  for (let i = 0; i < 3; i += 1) if (x[i] !== y[i]) return x[i] < y[i] ? -1 : 1;
  return 0;
}

function git(args, cwd, { allowFail = false, binary = false } = {}) {
  const result = spawnSync('git', ['-c', 'core.autocrlf=false', '-c', 'core.safecrlf=false', '-c', `core.hooksPath=${devNull}`, '-c', 'core.fsmonitor=false', ...args], {
    cwd,
    encoding: binary ? 'buffer' : 'utf8',
    maxBuffer: 512 * 1024 * 1024,
    windowsHide: true,
    env: {
      ...process.env,
      GIT_CONFIG_NOSYSTEM: '1',
      GIT_CONFIG_GLOBAL: devNull,
      GIT_TERMINAL_PROMPT: '0',
      GIT_AUTHOR_NAME: 'ojak-omp-update', GIT_AUTHOR_EMAIL: 'omp-update@invalid',
      GIT_COMMITTER_NAME: 'ojak-omp-update', GIT_COMMITTER_EMAIL: 'omp-update@invalid',
    },
  });
  if (result.error) throw result.error;
  const out = result.stdout ?? (binary ? Buffer.alloc(0) : '');
  const err = result.stderr?.toString() ?? '';
  if (result.status !== 0 && !allowFail) throw new Error(`git ${args[0]} failed (${result.status}): ${err.trim().split('\n').slice(-5).join(' | ')}`);
  return { status: result.status, out: binary ? out : out.trim(), err };
}

function loadPins(root) {
  const manifestPath = join(root, 'integrations/client-forks/manifest.json');
  const manifestText = readFileSync(manifestPath, 'utf8');
  const manifest = JSON.parse(manifestText);
  const index = manifest.clients?.findIndex(client => client.directory === CLIENT) ?? -1;
  const pinned = manifest.clients?.[index];
  if (manifest.version !== 1 || !pinned || !TAG.test(pinned.tag) || !SHA.test(pinned.revision) || !/^[a-f0-9]{64}$/.test(pinned.sha256)
    || !/^\d+\.\d+\.\d+-ojak\.[1-9]\d*$/.test(pinned.version) || typeof pinned.repository !== 'string') {
    throw new Refusal('manifest.json does not hold a valid pinned oh-my-pi fork');
  }
  return { manifestPath, manifestText, manifest, index, pinned, patchPath: join(root, 'integrations/client-forks/oh-my-pi.patch') };
}

/** Resolves `tag` on the remote to the commit it points at (peeling annotated tags) without cloning. */
function resolveRemoteTag(repository, tag) {
  const lines = git(['ls-remote', '--tags', '--', repository, `refs/tags/${tag}`, `refs/tags/${tag}^{}`], undefined).out.split('\n').filter(Boolean);
  const refs = new Map(lines.map(line => { const [sha, ref] = line.split(/\s+/); return [ref, sha]; }));
  const revision = refs.get(`refs/tags/${tag}^{}`) ?? refs.get(`refs/tags/${tag}`);
  if (!revision || !SHA.test(revision)) throw new Refusal(`Upstream has no tag ${tag}`);
  return revision;
}

function readVersion(file) { return JSON.parse(readFileSync(file, 'utf8')).version; }

function prepareCandidate(pins, tag, expectRevision, tmp) {
  const { pinned } = pins;
  const baseVersion = pinned.version.replace(/-ojak\.\d+$/, '');
  const newVersion = `${tag.slice(1)}-ojak.1`;
  const work = join(tmp, 'work');
  git(['clone', '-q', '--depth', '1', '--branch', tag, '--config', 'advice.detachedHead=false', '--', pinned.repository, work], undefined);
  const newRevision = git(['rev-parse', 'HEAD'], work).out;
  if (git(['rev-parse', `refs/tags/${tag}^{commit}`], work).out !== newRevision) throw new Refusal(`Tag ${tag} does not resolve to the cloned commit`);
  if (expectRevision && newRevision !== expectRevision) throw new Refusal(`Tag ${tag} resolves to ${newRevision}, expected ${expectRevision}; refusing a moved tag`);
  if (readVersion(join(work, 'packages/coding-agent/package.json')) !== tag.slice(1)) throw new Refusal(`Upstream ${tag} does not declare version ${tag.slice(1)}`);

  // Rebuild the pinned fork exactly as prepare-client-forks.mjs does, then normalize its version-only lines back to
  // upstream so a plain 3-way merge never conflicts on a version bump.
  const fetched = git(['fetch', '-q', '--depth', '1', 'origin', pinned.revision], work, { allowFail: true });
  if (fetched.status !== 0) throw new Refusal(`Cannot fetch pinned revision ${pinned.revision}: ${fetched.err.trim()}`);
  git(['checkout', '-q', '--detach', pinned.revision], work);
  if (readVersion(join(work, 'packages/coding-agent/package.json')) !== baseVersion) throw new Refusal('Pinned revision does not declare the pinned upstream version; manifest is inconsistent');
  const applied = git(['apply', '--index', '--', pins.patchPath], work, { allowFail: true });
  if (applied.status !== 0) throw new Refusal(`The pinned patch does not apply to its pinned base: ${applied.err.trim()}`);
  try { rebrandDirectory(work, baseVersion); } catch (error) {
    if (error instanceof RebrandError) throw new Refusal(`The pinned patch is not in the expected fork shape: ${error.message}`);
    throw error;
  }
  git(['add', '-A'], work);
  git(['commit', '-q', '--no-verify', '-m', 'normalized pinned fork'], work);
  const normalized = git(['rev-parse', 'HEAD'], work).out;

  git(['checkout', '-q', '--detach', newRevision], work);
  const picked = git(['cherry-pick', '--no-commit', normalized], work, { allowFail: true });
  if (picked.status !== 0) {
    const files = git(['diff', '--name-only', '--diff-filter=U'], work, { allowFail: true }).out.split('\n').filter(Boolean);
    git(['cherry-pick', '--quit'], work, { allowFail: true });
    throw new Conflict(`The pinned patch does not merge cleanly onto ${tag}`, files, `${picked.out}\n${picked.err}`.trim().split('\n').slice(-40).join('\n'));
  }
  try { rebrandDirectory(work, newVersion); } catch (error) {
    if (error instanceof RebrandError) throw new Conflict(`Upstream ${tag} changed the fork version lines: ${error.message}`, [...REBRAND_FILES]);
    throw error;
  }
  git(['add', '-A'], work);
  const mergedTree = git(['write-tree'], work).out;
  const patch = git(['diff', '--cached', '--binary', '--full-index', '--no-renames', '--no-ext-diff', '--src-prefix=a/', '--dst-prefix=b/'], work, { binary: true }).out;
  if (patch.length === 0) throw new Refusal('The candidate patch is empty');

  // Independent verification: the emitted bytes must reproduce the merged tree from a pristine checkout of the tag.
  git(['reset', '-q', '--hard', newRevision], work);
  git(['clean', '-fdxq'], work);
  const candidatePath = join(tmp, 'candidate.patch');
  writeFileSync(candidatePath, patch);
  const check = git(['apply', '--check', '--', candidatePath], work, { allowFail: true });
  if (check.status !== 0) throw new Refusal(`Candidate patch does not apply to ${tag}: ${check.err.trim()}`);
  git(['apply', '--', candidatePath], work);
  git(['add', '-A'], work);
  if (git(['write-tree'], work).out !== mergedTree) throw new Refusal('Candidate patch does not reproduce the merged tree');
  for (const file of REBRAND_FILES) {
    const text = readFileSync(join(work, file), 'utf8');
    if (!text.includes(newVersion)) throw new Refusal(`${file} does not carry ${newVersion}`);
  }
  if (readVersion(join(work, 'packages/coding-agent/package.json')) !== newVersion) throw new Refusal('Candidate coding-agent version is wrong');
  rebrandDirectory(work, tag.slice(1));
  if (git(['diff', '--name-only', newRevision, '--', ...REBRAND_FILES], work).out !== '') throw new Refusal('Fork version hunks are not limited to the rebrand files');
  return { patch, newRevision, newVersion };
}

function publish(pins, candidate, tag) {
  const directory = dirname(pins.patchPath);
  const patchTemp = join(directory, '.oh-my-pi.patch.candidate');
  const manifestTemp = join(directory, '.manifest.json.candidate');
  const oldPatch = readFileSync(pins.patchPath);
  const manifest = structuredClone(pins.manifest);
  Object.assign(manifest.clients[pins.index], { tag, revision: candidate.newRevision, version: candidate.newVersion, sha256: sha256(candidate.patch) });
  writeFileSync(patchTemp, candidate.patch);
  writeFileSync(manifestTemp, `${JSON.stringify(manifest, null, 2)}\n`);
  try {
    renameSync(patchTemp, pins.patchPath);
    renameSync(manifestTemp, pins.manifestPath);
  } catch (error) {
    writeFileSync(pins.patchPath, oldPatch);
    writeFileSync(pins.manifestPath, pins.manifestText);
    throw error;
  } finally {
    rmSync(patchTemp, { force: true });
    rmSync(manifestTemp, { force: true });
  }
}

export function updateOmpFork({ root = defaultRoot, tag, expectRevision }) {
  const pins = loadPins(root);
  const { pinned } = pins;
  const report = { status: 'error', pinnedTag: pinned.tag, pinnedRevision: pinned.revision, pinnedVersion: pinned.version, latestTag: tag, latestRevision: null, candidateVersion: null, conflicts: [], message: '', output: '' };
  const tmp = mkdtempSync(join(tmpdir(), 'ojak-omp-update-'));
  try {
    if (!parseTag(tag)) throw new Refusal(`Not a stable vX.Y.Z tag: ${tag}`);
    if (expectRevision && !SHA.test(expectRevision)) throw new Refusal('--expect-revision must be a 40-character commit SHA');
    if (sha256(readFileSync(pins.patchPath)) !== pinned.sha256) throw new Refusal('Pinned patch checksum mismatch; refusing to build on an untrusted patch');
    const order = compareTags(tag, pinned.tag);
    report.latestRevision = resolveRemoteTag(pinned.repository, tag);
    if (expectRevision && report.latestRevision !== expectRevision) throw new Refusal(`Tag ${tag} resolves to ${report.latestRevision}, expected ${expectRevision}; refusing a moved tag`);
    if (order < 0) return { ...report, status: 'not-newer', message: `Upstream ${tag} is older than the pinned ${pinned.tag}; nothing to do.` };
    if (order === 0) {
      if (report.latestRevision !== pinned.revision) throw new Refusal(`Pinned tag ${tag} moved from ${pinned.revision} to ${report.latestRevision}; refusing to follow it`);
      return { ...report, status: 'up-to-date', message: `Already pinned to ${tag}; nothing to do.` };
    }
    const candidate = prepareCandidate(pins, tag, expectRevision ?? report.latestRevision, tmp);
    report.latestRevision = candidate.newRevision;
    publish(pins, candidate, tag);
    return { ...report, status: 'updated', candidateVersion: candidate.newVersion, message: `Candidate ${candidate.newVersion} prepared from ${tag} (${candidate.newRevision}).` };
  } catch (error) {
    if (error instanceof Conflict) return { ...report, status: 'conflict', message: error.message, conflicts: error.files, output: error.output };
    if (error instanceof Refusal) return { ...report, status: 'refused', message: error.message };
    throw error;
  } finally {
    rmSync(tmp, { recursive: true, force: true });
  }
}

/**
 * Confirms the tree differs from `base` only by a well-formed oh-my-pi candidate: the patch plus the tag, revision,
 * version and sha256 of the oh-my-pi manifest entry. Nothing else may change, and the new pin must be internally
 * consistent and strictly newer. Used before a write token pushes it and again before a validation build runs.
 */
export function checkCandidate(root = defaultRoot, base = 'HEAD') {
  const pins = loadPins(root);
  const head = JSON.parse(git(['show', `${base}:integrations/client-forks/manifest.json`], root).out);
  const before = head.clients.find(client => client.directory === CLIENT);
  const changed = [...git(['diff', '--name-only', base], root).out.split('\n'), ...git(['ls-files', '--others', '--exclude-standard'], root).out.split('\n')].filter(Boolean).sort();
  const allowed = ['integrations/client-forks/manifest.json', 'integrations/client-forks/oh-my-pi.patch'];
  if (changed.length === 0) throw new Refusal('There is no candidate: nothing differs from the base');
  if (changed.some(file => !allowed.includes(file))) throw new Refusal(`Candidate touches files outside the fork pin: ${changed.join(', ')}`);
  const { pinned } = pins;
  const others = manifest => JSON.stringify({ ...manifest, clients: manifest.clients.filter(client => client.directory !== CLIENT) });
  if (others(head) !== others(pins.manifest)) throw new Refusal('Candidate changes manifest entries other than oh-my-pi');
  for (const key of Object.keys(before)) if (!['tag', 'revision', 'version', 'sha256'].includes(key) && before[key] !== pinned[key]) throw new Refusal(`Candidate changes oh-my-pi ${key}`);
  if (Object.keys(pinned).length !== Object.keys(before).length) throw new Refusal('Candidate changes the oh-my-pi entry shape');
  if (compareTags(pinned.tag, before.tag) <= 0) throw new Refusal('Candidate tag is not newer than the current pin');
  if (pinned.version !== `${pinned.tag.slice(1)}-ojak.1`) throw new Refusal('Candidate version must be <upstream version>-ojak.1');
  if (sha256(readFileSync(pins.patchPath)) !== pinned.sha256) throw new Refusal('Candidate manifest sha256 does not match oh-my-pi.patch');
  return { tag: pinned.tag, revision: pinned.revision, version: pinned.version };
}

export function renderIssue(report, { branch, prFailed = false } = {}) {
  const lines = [];
  let title;
  const lag = `Ojak is pinned to upstream \`${report.pinnedTag}\` (compatible omp \`${report.pinnedVersion}\`); upstream stable is \`${report.latestTag}\`.`;
  if (report.status === 'updated' && prFailed) {
    title = `omp upstream: candidate for oh-my-pi ${report.latestTag} needs a pull request`;
    lines.push(lag, '', `The candidate \`${report.candidateVersion}\` was prepared cleanly and pushed to branch \`${branch}\`, but the workflow could not open the pull request (Settings → Actions → "Allow GitHub Actions to create and approve pull requests" is off, or the token lacks pull-request write access). Open the PR from \`${branch}\` into \`main\` by hand. Validation runs were dispatched on the branch.`);
  } else if (report.status === 'conflict') {
    title = `omp upstream: oh-my-pi ${report.latestTag} conflicts with the Ojak patch`;
    lines.push(lag, '', `The pinned patch does not merge cleanly onto \`${report.latestTag}\`. Nothing was changed; the current compatible omp \`${report.pinnedVersion}\` stays the released one and is **not** the upstream latest.`, '', `Reason: ${report.message}`);
    if (report.conflicts?.length) lines.push('', 'Conflicting files:', ...report.conflicts.map(file => `- \`${file}\``));
    if (report.output) lines.push('', '<details><summary>git output</summary>', '', '```', report.output, '```', '</details>');
    lines.push('', 'To adopt it: rebase `integrations/client-forks/oh-my-pi.patch` onto the new tag by hand (keep the admission contract), update `manifest.json` (tag, revision, version `X.Y.Z-ojak.1`, sha256), then follow docs/release-policy.md.');
  } else if (report.status === 'refused') {
    title = `omp upstream: oh-my-pi ${report.latestTag} refused by the update guard`;
    lines.push(lag, '', `The automatic update refused to continue and changed nothing: ${report.message}`, '', 'Moved tags, checksum mismatches and an inconsistent pin are never followed automatically. Investigate before pinning anything.');
  } else {
    throw new Error(`No issue to render for status ${report.status}`);
  }
  lines.push('', 'Never publish a different upstream build as a fallback: the Ojak fork carries the durable-admission contract that the Ojak service requires.');
  return { title, body: lines.join('\n') };
}

function parseArgs(argv) {
  const options = {};
  for (let i = 0; i < argv.length; i += 1) {
    const arg = argv[i];
    if (['--check-candidate', '--pr-failed'].includes(arg)) options[arg.slice(2)] = true;
    else if (['--tag', '--expect-revision', '--report', '--root', '--render-issue', '--branch', '--base'].includes(arg) && argv[i + 1] !== undefined) { options[arg.slice(2)] = argv[i + 1]; i += 1; }
    else throw new Error(`Unknown or incomplete argument: ${arg}`);
  }
  return options;
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  try {
    const options = parseArgs(process.argv.slice(2));
    const root = options.root ? resolve(options.root) : defaultRoot;
    if (options['render-issue']) {
      process.stdout.write(`${JSON.stringify(renderIssue(JSON.parse(readFileSync(options['render-issue'], 'utf8')), { branch: options.branch, prFailed: Boolean(options['pr-failed']) }))}\n`);
    } else if (options['check-candidate']) {
      console.log(JSON.stringify(checkCandidate(root, options.base ?? 'HEAD')));
    } else {
      if (!options.tag) throw new Error('Usage: node scripts/update-omp-fork.mjs --tag vX.Y.Z [--expect-revision <sha>] [--report <file>] [--root <dir>]');
      const report = updateOmpFork({ root, tag: options.tag, expectRevision: options['expect-revision'] });
      if (options.report) writeFileSync(resolve(options.report), `${JSON.stringify(report, null, 2)}\n`);
      console.log(`${report.status}: ${report.message}`);
      if (report.conflicts.length) console.log(`conflicts: ${report.conflicts.join(', ')}`);
      process.exit(report.status === 'conflict' ? 10 : report.status === 'refused' ? 11 : 0);
    }
  } catch (error) {
    if (error instanceof Refusal) { console.error(error.message); process.exit(11); }
    console.error(error.message ?? error);
    process.exit(1);
  }
}
