import { spawnSync } from 'node:child_process';
import { existsSync, mkdirSync, copyFileSync, writeFileSync, readFileSync } from 'node:fs';
import { homedir } from 'node:os';
import { resolve, dirname, delimiter } from 'node:path';
import { fileURLToPath } from 'node:url';
import { createHash } from 'node:crypto';
const root = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const windows = process.platform === 'win32';
const exe = windows ? '.exe' : '';
const verification = process.argv.slice(2);
const verifyBuild = verification.length === 1 && verification[0] === '--verify-build';
if (verification.length && !verifyBuild) throw new Error('Unsupported build option.');
if (verifyBuild && process.env.GITHUB_ACTIONS === 'true'
    && (!['push', 'pull_request'].includes(process.env.GITHUB_EVENT_NAME)
      || !/^(refs\/heads\/|refs\/pull\/)/.test(process.env.GITHUB_REF ?? ''))) {
  throw new Error('Unsigned verification is allowed only on branch/PR CI, never release tags.');
}
if (verifyBuild && (process.env.TAURI_SIGNING_PRIVATE_KEY || process.env.TAURI_SIGNING_PRIVATE_KEY_PATH)) {
  throw new Error('Verification builds must not receive release signing keys.');
}
const cargo = existsSync(resolve(homedir(), `.cargo/bin/cargo${exe}`)) ? resolve(homedir(), `.cargo/bin/cargo${exe}`) : 'cargo';
function run(cmd, args, cwd = root) {
  // Windows의 .cmd 실행에는 셸이 필요하다. 인수는 이 스크립트가 만든 고정값뿐이다.
  const child = spawnSync(cmd, args, { cwd, stdio: 'inherit', shell: windows && cmd.endsWith('.cmd'), env: { ...process.env, PATH: `${resolve(homedir(), '.cargo/bin')}${delimiter}${process.env.PATH}` } });
  if (child.error) throw child.error;
  if (child.status !== 0) process.exit(child.status ?? 1);
}
run(cargo, ['build', '--release', '-p', 'aam-service', '-p', 'aam-launcher']);
const rustc = existsSync(resolve(homedir(), `.cargo/bin/rustc${exe}`)) ? resolve(homedir(), `.cargo/bin/rustc${exe}`) : 'rustc';
const version = spawnSync(rustc, ['-vV'], { cwd: root, encoding: 'utf8' });
if (version.status !== 0) throw new Error('Rust target triple을 확인하지 못했습니다.');
const triple = version.stdout.match(/^host: (\S+)$/m)?.[1];
if (!triple || !/^[a-z0-9_-]+$/.test(triple)) throw new Error('Rust target triple 형식이 올바르지 않습니다.');
const native = resolve(root, 'apps/desktop/src-tauri');
mkdirSync(resolve(native, 'binaries'), { recursive: true });
// Tauri sidecar 이름 규칙: <이름>-<target triple>[.exe]
for (const name of ['aam', 'aam-service']) copyFileSync(resolve(root, 'target/release', `${name}${exe}`), resolve(native, 'binaries', `${name}-${triple}${exe}`));
// NSIS needs the new helper before replacing an older installation, and verifies both installed sidecars against this payload.
if (windows) for (const name of ['aam', 'aam-service']) copyFileSync(resolve(root, 'target/release', `${name}.exe`), resolve(native, 'binaries', `installer-${name}.exe`));
// Keep the exact service image and matching PDB outside the installer payload.
// WER dumps are private runtime data and are never included here.
if (windows) {
  const appVersion = JSON.parse(readFileSync(resolve(root, 'package.json'), 'utf8')).version;
  const symbols = resolve(root, 'target/symbols', appVersion);
  mkdirSync(symbols, { recursive: true });
  const checksums = [];
  for (const file of ['aam-service.exe', 'aam_service.pdb']) {
    const source = resolve(root, 'target/release', file);
    const bytes = readFileSync(source);
    const hash = createHash('sha256').update(bytes).digest('hex');
    const destination = resolve(symbols, file);
    if (existsSync(destination) && createHash('sha256').update(readFileSync(destination)).digest('hex') !== hash) {
      throw new Error(`Refusing to replace different ${appVersion} service symbols; bump the version first.`);
    }
    copyFileSync(source, destination);
    checksums.push(`${hash}  ${file}`);
  }
  writeFileSync(resolve(symbols, 'SHA256SUMS'), `${checksums.join('\n')}\n`);
}
const tauriArgs = ['build'];
// createUpdaterArtifacts needs a private key that matches plugins.updater.pubkey.
// Local builds omit the key, so skip updater archives instead of failing the app bundle.
// Release builds must sign. Explicit branch/PR verification never creates updater artifacts.
if (!process.env.TAURI_SIGNING_PRIVATE_KEY && !process.env.TAURI_SIGNING_PRIVATE_KEY_PATH) {
  if (process.env.GITHUB_ACTIONS === 'true' && !verifyBuild) {
    throw new Error('GitHub Actions builds require TAURI_SIGNING_PRIVATE_KEY so updater artifacts are signed. Refusing to skip them.');
  }
  // 인라인 JSON은 Windows .cmd 셸 인수에서 따옴표가 사라지므로 파일로 넘긴다.
  const override = resolve(native, 'tauri.local-build.conf.json');
  writeFileSync(override, JSON.stringify({ bundle: { createUpdaterArtifacts: false } }));
  tauriArgs.push('--config', override);
}
run(resolve(root, `node_modules/.bin/tauri${windows ? '.cmd' : ''}`), tauriArgs, resolve(root, 'apps/desktop'));
