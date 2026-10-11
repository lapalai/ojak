// Consumer-behavior tests for install.sh. install.sh runs for real (/bin/sh); only the external
// macOS/network tools are replaced by fixtures through the marked "external tools" block, and every
// path (HOME, AAM_HOME, /Applications) lives under a temp directory. The user's real Ojak.app, service
// and network are never touched. Run: node --test scripts/install-sh.test.mjs
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { chmod, mkdir, mkdtemp, readdir, readFile, rm, stat, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { spawnSync } from 'node:child_process';
import test from 'node:test';
import { fileURLToPath } from 'node:url';

const darwin = process.platform === 'darwin';
const installer = await readFile(fileURLToPath(new URL('../install.sh', import.meta.url)), 'utf8');
const REPO = 'lapalai/ojak';

const AAM_SCRIPT = (version, archs) => `#!/bin/sh
# ARCHS=${archs}
VERSION=${version}
S="$FAKE_STATE"
echo "$VERSION aam $*" >> "$S/calls.log"
case "$*" in
  --version) echo "aam $VERSION" ;;
  "status --json")
    if [ ! -e "$S/service_running" ]; then echo "aam: 서비스에 연결하지 못했어요 (DAEMON_UNAVAILABLE)" >&2; exit 1; fi
    if [ -e "$S/status_broken" ]; then echo "aam: boom (INTERNAL)" >&2; exit 1; fi
    cat "$S/sessions.json" ;;
  "service stop")
    if [ -e "$S/stop_busy" ]; then echo "aam: 대화가 남아 있어요 (SESSION_BUSY)" >&2; exit 1; fi
    if [ -e "$S/stop_fail" ]; then echo "aam: 끄지 못했어요 (SERVICE_BOOTOUT_FAILED)" >&2; exit 1; fi
    rm -f "$S/service_running"; echo "stopped" ;;
  "service install")
    if [ -e "$S/fail_service_install" ] && grep -qx "$VERSION" "$S/fail_service_install"; then
      echo "aam: 등록 실패 (SERVICE_BOOTSTRAP_FAILED)" >&2; exit 1
    fi
    touch "$S/service_running"; mkdir -p "$AAM_HOME"; echo '{}' > "$AAM_HOME/service-install.json"; echo "service installed" ;;
  "setup"|"setup --with-omp")
    if [ -e "$S/setup_fail" ]; then echo "aam: setup 실패 (SETUP_FAILED)" >&2; exit 1; fi
    touch "$S/service_running"; mkdir -p "$AAM_HOME"; echo '{}' > "$AAM_HOME/service-install.json"
    if [ -f "$S/setup.json" ]; then cat "$S/setup.json"; else echo '{"ready":false}'; fi ;;
  "setup --check") cat "$S/check.json" ;;
  "integration install"|"shell install"|"deactivate"|"deactivate --dry-run") echo "ok $*" ;;
  *) echo "unexpected aam $*" >&2; exit 2 ;;
esac
`;

const FAKES = {
  curl: `#!/bin/sh
out=""; url=""
while [ $# -gt 0 ]; do
  case "$1" in -o) out=$2; shift ;; https://*) url=$1 ;; esac
  shift
done
echo "curl $url" >> "$FAKE_STATE/calls.log"
src=""
case "$url" in
  https://api.github.com/repos/*/releases/latest) src="$FAKE_FIX/latest.json" ;;
  https://api.github.com/repos/*/releases/tags/*) src="$FAKE_FIX/tag-\${url##*/}.json" ;;
  https://github.com/*)
    rest=\${url%/*}; tag=\${rest##*/}
    src="$FAKE_FIX/assets/$tag/\${url##*/}" ;;
esac
[ -f "$src" ] || exit 22
cp "$src" "$out"
`,
  hdiutil: `#!/bin/sh
echo "hdiutil $1" >> "$FAKE_STATE/calls.log"
if [ "$1" = attach ]; then
  mp=""; file=""
  while [ $# -gt 0 ]; do
    case "$1" in -mountpoint) mp=$2; shift ;; attach|-nobrowse|-readonly) ;; *) file=$1 ;; esac
    shift
  done
  [ -e "$FAKE_STATE/attach_fail" ] && exit 1
  mkdir -p "$mp"
  /bin/cp -R "$(cat "$file")" "$mp/Ojak.app"
fi
exit 0
`,
  ditto: `#!/bin/sh
echo "ditto" >> "$FAKE_STATE/calls.log"
if [ -e "$FAKE_STATE/ditto_partial" ]; then
  mkdir -p "$2/Contents"; echo partial > "$2/Contents/partial.bin"; exit 1
fi
exec /usr/bin/ditto "$@"
`,
  codesign: `#!/bin/sh
echo "codesign $*" >> "$FAKE_STATE/calls.log"
for last; do :; done
[ -e "$last/Contents/BAD_SIGNATURE" ] && exit 1
exit 0
`,
  lipo: `#!/bin/sh
for last; do :; done
sed -n 's/^# ARCHS=//p' "$last" | head -n 1
`,
  uname: `#!/bin/sh
case "$1" in -s) echo "\${FAKE_UNAME_S:-Darwin}" ;; -m) echo "\${FAKE_UNAME_M:-arm64}" ;; esac
`,
  sysctl: `#!/bin/sh
echo "\${FAKE_SYSCTL_ARM64:-0}"
`,
  sw_vers: `#!/bin/sh
echo "\${FAKE_MACOS:-14.5}"
`,
  sudo: `#!/bin/sh
echo "sudo $*" >> "$FAKE_STATE/calls.log"
exec "$@"
`,
};

const CHECK_OK = {
  ready: true,
  serviceVersionMismatch: false,
  configured: true,
  tools: [
    { tool: 'claude', account: true, connected: true, verified: true },
    { tool: 'codex', account: false, connected: false, verified: null },
  ],
  notices: [],
};
const OMP_OK = { broker: true, bridge: true, observer: true, error: null, futureLaunchesOnly: true };

async function makeApp(dir, { version, archs = 'arm64', badSignature = false, plistVersion = version }) {
  const macos = join(dir, 'Contents', 'MacOS');
  await mkdir(macos, { recursive: true });
  await writeFile(
    join(dir, 'Contents', 'Info.plist'),
    `<?xml version="1.0" encoding="UTF-8"?>\n<plist version="1.0"><dict><key>CFBundleShortVersionString</key><string>${plistVersion}</string></dict></plist>\n`,
  );
  for (const name of ['aam', 'aam-service', 'ai-account-manager']) {
    const file = join(macos, name);
    await writeFile(file, name === 'aam' ? AAM_SCRIPT(version, archs) : `#!/bin/sh\n# ARCHS=${archs}\n`);
    await chmod(file, 0o755);
  }
  if (badSignature) await writeFile(join(dir, 'Contents', 'BAD_SIGNATURE'), 'x');
}

async function sandbox(t) {
  const root = await mkdtemp(join(tmpdir(), 'ojak-install-test-'));
  t.after(() => rm(root, { recursive: true, force: true }));
  const sb = {
    root,
    home: join(root, 'home'),
    apps: join(root, 'home', 'Applications'),
    sysApps: join(root, 'sysapps'),
    aamHome: join(root, 'aamhome'),
    fake: join(root, 'fake'),
    state: join(root, 'state'),
    fix: join(root, 'fix'),
    tmp: join(root, 'tmp'),
    script: join(root, 'install.sh'),
  };
  for (const dir of [sb.home, sb.apps, sb.sysApps, sb.fake, sb.state, sb.fix, sb.tmp]) await mkdir(dir, { recursive: true });
  for (const [name, body] of Object.entries(FAKES)) {
    await writeFile(join(sb.fake, name), body);
    await chmod(join(sb.fake, name), 0o755);
  }
  const block = [
    `CURL=${sb.fake}/curl`, 'SHASUM=/usr/bin/shasum', `HDIUTIL=${sb.fake}/hdiutil`, `DITTO=${sb.fake}/ditto`,
    `CODESIGN=${sb.fake}/codesign`, `LIPO=${sb.fake}/lipo`, 'XATTR=/usr/bin/xattr', 'PLUTIL=/usr/bin/plutil',
    `UNAME=${sb.fake}/uname`, `SYSCTL=${sb.fake}/sysctl`, `SW_VERS=${sb.fake}/sw_vers`, `SUDO=${sb.fake}/sudo`,
    `SYSTEM_APPS=${sb.sysApps}`,
  ].join('\n');
  const start = installer.indexOf('# --- external tools');
  const end = installer.indexOf('# --- end external tools ---');
  assert.ok(start > 0 && end > start, 'install.sh must keep the replaceable external tools block');
  await writeFile(sb.script, `${installer.slice(0, start)}${block}\n${installer.slice(end)}`);
  await writeFile(join(sb.state, 'calls.log'), '');
  await writeFile(join(sb.state, 'sessions.json'), JSON.stringify({ sessions: [] }));
  await writeFile(join(sb.state, 'check.json'), JSON.stringify(CHECK_OK));
  sb.setCheck = (patch) => writeFile(join(sb.state, 'check.json'), JSON.stringify({ ...CHECK_OK, ...patch }));
  sb.flag = (name, body = '1') => writeFile(join(sb.state, name), body);
  sb.run = (args = [], env = {}) => {
    const result = spawnSync('/bin/sh', [sb.script, ...args], {
      env: {
        PATH: '/usr/bin:/bin', HOME: sb.home, AAM_HOME: sb.aamHome, TMPDIR: sb.tmp,
        FAKE_STATE: sb.state, FAKE_FIX: sb.fix, ...env,
      },
      encoding: 'utf8',
      cwd: sb.root,
    });
    result.out = `${result.stdout}${result.stderr}`;
    return result;
  };
  sb.calls = async () => (await readFile(join(sb.state, 'calls.log'), 'utf8')).split('\n').filter(Boolean);
  sb.clearCalls = () => writeFile(join(sb.state, 'calls.log'), '');
  sb.appVersion = async (apps = sb.apps) => {
    const text = await readFile(join(apps, 'Ojak.app', 'Contents', 'MacOS', 'aam'), 'utf8');
    return /^VERSION=(.+)$/m.exec(text)[1];
  };
  sb.running = () => stat(join(sb.state, 'service_running')).then(() => true, () => false);
  sb.entries = async (dir = sb.apps) => (await readdir(dir)).sort();
  return sb;
}

/** Publish a release the fake curl/hdiutil serve. `latest` also becomes the "latest" endpoint. */
async function publish(sb, version, options = {}) {
  const { latest = true, wrongHash = false, assetUrlRepo = REPO, draft = false, tagVersion = version, ...appOptions } = options;
  const appDir = join(sb.fix, `app-${version}`);
  await makeApp(appDir, { version, ...appOptions });
  const tag = `v${tagVersion}`;
  const dmgName = `Ojak_${version}_aarch64.dmg`;
  const assetDir = join(sb.fix, 'assets', tag);
  await mkdir(assetDir, { recursive: true });
  const dmg = appDir; // the fake hdiutil reads this "DMG" as a pointer to the fixture app
  await writeFile(join(assetDir, dmgName), dmg);
  const hash = wrongHash ? '0'.repeat(64) : createHash('sha256').update(dmg).digest('hex');
  await writeFile(join(assetDir, 'SHA256SUMS'), `${hash}  ${dmgName}\n`);
  const base = `https://github.com/${assetUrlRepo}/releases/download/${tag}`;
  const json = JSON.stringify({
    tag_name: tag, draft, prerelease: false,
    assets: [
      { name: dmgName, browser_download_url: `${base}/${dmgName}` },
      { name: 'SHA256SUMS', browser_download_url: `${base}/SHA256SUMS` },
    ],
  }, null, 2);
  await writeFile(join(sb.fix, `tag-${tag}.json`), json);
  if (latest) await writeFile(join(sb.fix, 'latest.json'), json);
  return { tag, dmgName };
}

/** An already-installed Ojak with its fake service. */
async function installOld(sb, version, { running = true, sessions = [], apps = sb.apps, ...appOptions } = {}) {
  await makeApp(join(apps, 'Ojak.app'), { version, ...appOptions });
  if (running) await sb.flag('service_running');
  await mkdir(sb.aamHome, { recursive: true });
  await writeFile(join(sb.aamHome, 'service-install.json'), '{}');
  await writeFile(join(sb.state, 'sessions.json'), JSON.stringify({ sessions: sessions.map((state) => ({ state })) }));
}

const resultLine = (out) => /^OJAK_INSTALL_RESULT=(.+)$/m.exec(out)?.[1];
const has = (calls, text) => calls.some((line) => line.includes(text));
const at = (calls, text) => calls.findIndex((line) => line.includes(text));

const opts = { skip: !darwin && 'install.sh tests need macOS plutil/ditto' };

test('help documents every flag and contacts nothing', opts, async (t) => {
  const sb = await sandbox(t);
  const result = sb.run(['--help']);
  assert.equal(result.status, 0);
  for (const flag of ['--version', '--with-omp', '--dry-run', '--help', '--system', '--skip-service', '--skip-integration', '--skip-shell', '--uninstall', '--clear-quarantine']) {
    assert.ok(result.stdout.includes(flag), `help mentions ${flag}`);
  }
  assert.match(result.stdout, /exit 3|OJAK_INSTALL_RESULT/);
  assert.deepEqual(await sb.calls(), []);
});

test('usage errors exit 2 and change nothing', opts, async (t) => {
  const sb = await sandbox(t);
  for (const args of [['--bogus'], ['--version'], ['--version', '0.5'], ['--version', '1.0.0-rc.1'], ['--with-omp', '--skip-shell'], ['--uninstall', '--with-omp']]) {
    const result = sb.run(args);
    assert.equal(result.status, 2, `${args.join(' ')}: ${result.out}`);
  }
  assert.deepEqual(await sb.calls(), []);
  assert.deepEqual(await sb.entries(), []);
});

test('unsupported architecture and old macOS are refused before any network or file change', opts, async (t) => {
  const sb = await sandbox(t);
  await publish(sb, '0.5.8');
  let result = sb.run([], { FAKE_UNAME_M: 'x86_64', FAKE_SYSCTL_ARM64: '0' });
  assert.equal(result.status, 1);
  assert.match(result.out, /Apple silicon \(arm64\)/);
  result = sb.run([], { FAKE_UNAME_S: 'Linux' });
  assert.equal(result.status, 1);
  assert.match(result.out, /macOS only/);
  result = sb.run([], { FAKE_MACOS: '12.7' });
  assert.equal(result.status, 1);
  assert.match(result.out, /macOS 13/);
  assert.deepEqual(await sb.calls(), []);
  assert.deepEqual(await sb.entries(), []);
  // A shell under Rosetta reports x86_64 on Apple silicon hardware and must still be accepted.
  result = sb.run(['--dry-run'], { FAKE_UNAME_M: 'x86_64', FAKE_SYSCTL_ARM64: '1' });
  assert.equal(result.status, 0, result.out);
});

test('a DMG whose app is not arm64, is mis-versioned, or fails the signature check is rejected untouched', opts, async (t) => {
  for (const [label, appOptions, message] of [
    ['x86_64-only binaries', { archs: 'x86_64' }, /not an arm64 binary/],
    ['mismatched bundle version', { plistVersion: '0.5.7' }, /reports version/],
    ['bad code signature', { badSignature: true }, /signature does not verify/],
  ]) {
    const sb = await sandbox(t);
    await installOld(sb, '0.5.7');
    await publish(sb, '0.5.8', appOptions);
    const result = sb.run();
    assert.equal(result.status, 1, `${label}: ${result.out}`);
    assert.match(result.out, message, label);
    const calls = await sb.calls();
    assert.ok(!has(calls, 'service stop'), `${label}: service untouched`);
    assert.equal(await sb.appVersion(), '0.5.7', label);
    assert.ok(await sb.running(), label);
    assert.deepEqual(await sb.entries(), ['Ojak.app'], `${label}: staging removed`);
  }
});

test('hash mismatch and missing SHA256 line stop before mounting; the existing install is kept', opts, async (t) => {
  const sb = await sandbox(t);
  await installOld(sb, '0.5.7');
  await publish(sb, '0.5.8', { wrongHash: true });
  const result = sb.run();
  assert.equal(result.status, 1);
  assert.match(result.out, /SHA256 mismatch/);
  const calls = await sb.calls();
  assert.ok(!has(calls, 'hdiutil'), 'never mounts an unverified DMG');
  assert.ok(!has(calls, 'service stop'));
  assert.equal(await sb.appVersion(), '0.5.7');
  assert.deepEqual(await sb.entries(), ['Ojak.app']);
  await writeFile(join(sb.fix, 'assets', 'v0.5.8', 'SHA256SUMS'), 'not a hash  other.dmg\n');
  const missing = sb.run();
  assert.equal(missing.status, 1);
  assert.match(missing.out, /no valid SHA-256 line/);
});

test('release assets must come from this repo and the pinned tag; drafts and mismatched tags are refused', opts, async (t) => {
  let sb = await sandbox(t);
  await publish(sb, '0.5.8', { assetUrlRepo: 'evil/ojak' });
  let result = sb.run();
  assert.equal(result.status, 1);
  assert.match(result.out, /not an lapalai\/ojak/);
  sb = await sandbox(t);
  await publish(sb, '0.5.8', { draft: true });
  result = sb.run();
  assert.equal(result.status, 1);
  assert.match(result.out, /draft or pre-release/);
  sb = await sandbox(t);
  await publish(sb, '0.5.8', { latest: false });
  await publish(sb, '0.5.9', { latest: false, tagVersion: '0.5.9' });
  await writeFile(join(sb.fix, 'tag-v0.5.8.json'), await readFile(join(sb.fix, 'tag-v0.5.9.json'), 'utf8'));
  result = sb.run(['--version', '0.5.8']);
  assert.equal(result.status, 1);
  assert.match(result.out, /Asked for 0\.5\.8 but GitHub returned 0\.5\.9/);
});

test('--dry-run changes nothing and shows the plan; --version pins the tag endpoint', opts, async (t) => {
  const sb = await sandbox(t);
  await publish(sb, '0.5.8');
  await publish(sb, '0.5.7', { latest: false });
  const latest = sb.run(['--dry-run', '--with-omp']);
  assert.equal(latest.status, 0, latest.out);
  assert.match(latest.stdout, /release:\s+v0\.5\.8/);
  assert.match(latest.stdout, /setup --with-omp/);
  assert.equal(resultLine(latest.stdout), 'dry-run');
  let calls = await sb.calls();
  assert.deepEqual(calls, [`curl https://api.github.com/repos/${REPO}/releases/latest`], 'only the release metadata is read');
  assert.deepEqual(await sb.entries(), []);
  assert.deepEqual(await readdir(sb.tmp), [], 'no temp files left');
  await sb.clearCalls();
  const pinned = sb.run(['--dry-run', '--version', 'v0.5.7']);
  assert.equal(pinned.status, 0, pinned.out);
  assert.match(pinned.stdout, /release:\s+v0\.5\.7/);
  calls = await sb.calls();
  assert.deepEqual(calls, [`curl https://api.github.com/repos/${REPO}/releases/tags/v0.5.7`]);
});

test('dry-run on a busy machine reports the block without stopping anything', opts, async (t) => {
  const sb = await sandbox(t);
  await installOld(sb, '0.5.7', { sessions: ['ACTIVE'] });
  await publish(sb, '0.5.8');
  const result = sb.run(['--dry-run']);
  assert.equal(result.status, 0, result.out);
  assert.match(result.stdout, /BLOCKED/);
  const calls = await sb.calls();
  assert.ok(!has(calls, 'service stop') && !has(calls, 'hdiutil'));
  assert.equal(await sb.appVersion(), '0.5.7');
  assert.ok(await sb.running());
});

test('new install: default setup, no omp, ready is judged from setup --check', opts, async (t) => {
  const sb = await sandbox(t);
  await publish(sb, '0.5.8');
  const result = sb.run();
  assert.equal(result.status, 0, result.out);
  assert.equal(resultLine(result.out), 'ready');
  assert.equal(await sb.appVersion(), '0.5.8');
  assert.deepEqual(await sb.entries(), ['Ojak.app'], 'no staging or previous leftovers');
  const calls = await sb.calls();
  assert.ok(calls.includes('0.5.8 aam setup'));
  assert.ok(calls.includes('0.5.8 aam setup --check'));
  assert.ok(!has(calls, '--with-omp') && !has(calls, 'service stop') && !has(calls, 'omp-'), 'omp is never touched without opt-in');
  assert.ok(at(calls, 'aam setup') > at(calls, 'hdiutil detach'));
  assert.match(result.out, /not notarized/);
  assert.ok(!has(calls, 'xattr'), 'quarantine is left alone by default');
});

test('update over an idle install: stop with the OLD aam, replace, restart with the NEW aam, keep nothing behind', opts, async (t) => {
  const sb = await sandbox(t);
  await installOld(sb, '0.5.7', { sessions: ['EXITED', 'RELEASED'] });
  await publish(sb, '0.5.8');
  const result = sb.run();
  assert.equal(result.status, 0, result.out);
  assert.equal(resultLine(result.out), 'ready');
  const calls = await sb.calls();
  const stop = at(calls, '0.5.7 aam service stop');
  const swapInstall = at(calls, '0.5.8 aam service install');
  assert.ok(at(calls, '0.5.7 aam status --json') >= 0 && at(calls, '0.5.7 aam status --json') < stop, 'session probe precedes the stop');
  assert.ok(stop > at(calls, 'hdiutil detach'), 'staging is verified before the service is stopped');
  assert.ok(swapInstall > stop);
  assert.ok(at(calls, '0.5.8 aam setup') > swapInstall);
  assert.equal(await sb.appVersion(), '0.5.8');
  assert.deepEqual(await sb.entries(), ['Ojak.app']);
  assert.ok(await sb.running());
});

test('busy or uncertain sessions refuse the update (exit 3) and nothing is killed or replaced', opts, async (t) => {
  for (const [label, prepare] of [
    ['active session', (sb) => sb.flag('sessions.json', JSON.stringify({ sessions: [{ state: 'ACTIVE' }] }))],
    ['starting session', (sb) => sb.flag('sessions.json', JSON.stringify({ sessions: [{ state: 'STARTING' }] }))],
    ['orphaned session', (sb) => sb.flag('sessions.json', JSON.stringify({ sessions: [{ state: 'ORPHANED' }] }))],
    ['status unreadable', (sb) => sb.flag('status_broken')],
  ]) {
    const sb = await sandbox(t);
    await installOld(sb, '0.5.7');
    await publish(sb, '0.5.8');
    await prepare(sb);
    const result = sb.run();
    assert.equal(result.status, 3, `${label}: ${result.out}`);
    assert.equal(resultLine(result.out), 'refused-busy', label);
    const calls = await sb.calls();
    assert.ok(!has(calls, 'service stop'), `${label}: no stop`);
    assert.ok(!calls.some((line) => line.includes('releases/download')), `${label}: nothing downloaded`);
    assert.equal(await sb.appVersion(), '0.5.7', label);
    assert.ok(await sb.running(), `${label}: service still up`);
    assert.deepEqual(await sb.entries(), ['Ojak.app'], label);
  }
});

test('a session that starts after the probe is caught by `aam service stop` (SESSION_BUSY) and the app is kept', opts, async (t) => {
  const sb = await sandbox(t);
  await installOld(sb, '0.5.7');
  await publish(sb, '0.5.8');
  await sb.flag('stop_busy');
  const result = sb.run();
  assert.equal(result.status, 3, result.out);
  assert.equal(resultLine(result.out), 'refused-busy');
  assert.equal(await sb.appVersion(), '0.5.7');
  assert.deepEqual(await sb.entries(), ['Ojak.app'], 'verified staging copy is discarded');
  assert.ok(await sb.running());
  assert.ok(!has(await sb.calls(), '0.5.8 aam service install'));
});

test('other stop failures are also a safe refusal, not a forced replacement', opts, async (t) => {
  const sb = await sandbox(t);
  await installOld(sb, '0.5.7');
  await publish(sb, '0.5.8');
  await sb.flag('stop_fail');
  const result = sb.run();
  assert.equal(result.status, 3, result.out);
  assert.equal(resultLine(result.out), 'refused-uncertain');
  assert.equal(await sb.appVersion(), '0.5.7');
  assert.deepEqual(await sb.entries(), ['Ojak.app']);
});

test('a copy that fails part-way is discarded and neither the app nor the service is touched', opts, async (t) => {
  const sb = await sandbox(t);
  await installOld(sb, '0.5.7');
  await publish(sb, '0.5.8');
  await sb.flag('ditto_partial');
  const result = sb.run();
  assert.equal(result.status, 1, result.out);
  assert.match(result.out, /failed part-way/);
  assert.ok(!has(await sb.calls(), 'service stop'));
  assert.equal(await sb.appVersion(), '0.5.7');
  assert.deepEqual(await sb.entries(), ['Ojak.app'], 'partial staging removed');
  assert.ok(await sb.running());
  // A partial copy on a fresh machine must not leave a half app either.
  const fresh = await sandbox(t);
  await publish(fresh, '0.5.8');
  await fresh.flag('ditto_partial');
  assert.equal(fresh.run().status, 1);
  assert.deepEqual(await fresh.entries(), []);
});

test('if the new service cannot start, the previous app is restored and the previous service restarted', opts, async (t) => {
  const sb = await sandbox(t);
  await installOld(sb, '0.5.7');
  await publish(sb, '0.5.8');
  await sb.flag('fail_service_install', '0.5.8\n');
  const result = sb.run();
  assert.equal(result.status, 1, result.out);
  assert.equal(resultLine(result.out), 'rolled-back');
  assert.match(result.out, /Restored the previous Ojak\.app/);
  const calls = await sb.calls();
  assert.ok(at(calls, '0.5.7 aam service install') > at(calls, '0.5.8 aam service install'), 'old service restarted after the failure');
  assert.ok(!calls.includes('0.5.8 aam setup'));
  assert.equal(await sb.appVersion(), '0.5.7');
  assert.deepEqual(await sb.entries(), ['Ojak.app'], 'no .previous or staging left');
  assert.ok(await sb.running());
});

test('rollback that cannot restart the old service says so loudly (exit 6) but still restores the app', opts, async (t) => {
  const sb = await sandbox(t);
  await installOld(sb, '0.5.7');
  await publish(sb, '0.5.8');
  await sb.flag('fail_service_install', '0.5.8\n0.5.7\n');
  const result = sb.run();
  assert.equal(result.status, 6, result.out);
  assert.equal(resultLine(result.out), 'rollback-failed');
  assert.match(result.out, /service is STOPPED/);
  assert.equal(await sb.appVersion(), '0.5.7');
  assert.ok(!(await sb.running()));
});

test('a failed verification of the installed copy also rolls back', opts, async (t) => {
  const sb = await sandbox(t);
  await installOld(sb, '0.5.7');
  await publish(sb, '0.5.8');
  // First codesign (staging) passes; the installed copy fails: simulate with a signature check keyed on the final path.
  await writeFile(join(sb.fake, 'codesign'), `#!/bin/sh
for last; do :; done
case "$last" in */.Ojak.app.installing) exit 0 ;; *) exit 1 ;; esac
`);
  const result = sb.run();
  assert.equal(result.status, 1, result.out);
  assert.equal(resultLine(result.out), 'rolled-back');
  assert.equal(await sb.appVersion(), '0.5.7');
  assert.ok(await sb.running());
});

test('setup --check ready=false after a successful copy is "installed, setup incomplete", not success', opts, async (t) => {
  const sb = await sandbox(t);
  await publish(sb, '0.5.8');
  await sb.setCheck({
    ready: false,
    tools: [{ tool: 'claude', account: true, connected: true, verified: false }],
    notices: [{ step: 'terminal', tool: 'claude', code: 'command-mismatch', message: 'claude 명령이 관리 shim과 다릅니다.' }],
  });
  const result = sb.run();
  assert.equal(result.status, 4, result.out);
  assert.equal(resultLine(result.out), 'installed-not-ready');
  assert.match(result.out, /Installed, setup incomplete/);
  assert.match(result.out, /command-mismatch/);
  assert.equal(await sb.appVersion(), '0.5.8', 'files stay installed');
  assert.ok(!/Ojak is installed and verified/.test(result.out));
});

test('ready=true with serviceVersionMismatch=true is still rejected', opts, async (t) => {
  const sb = await sandbox(t);
  await publish(sb, '0.5.8');
  await sb.setCheck({ ready: true, serviceVersionMismatch: true });
  const result = sb.run();
  assert.equal(result.status, 4, result.out);
  assert.equal(resultLine(result.out), 'installed-not-ready');
  assert.match(result.out, /serviceVersionMismatch is 'true'/);
  assert.match(result.out, /service restart/);
});

test('a missing serviceVersionMismatch field (older aam) is not treated as fine', opts, async (t) => {
  const sb = await sandbox(t);
  await publish(sb, '0.5.8');
  const { serviceVersionMismatch, ...legacy } = CHECK_OK;
  void serviceVersionMismatch;
  await writeFile(join(sb.state, 'check.json'), JSON.stringify(legacy));
  const result = sb.run();
  assert.equal(result.status, 4, result.out);
  assert.match(result.out, /serviceVersionMismatch is 'missing'/);
});

test('no runnable account is reported as an action for the user, not as ready', opts, async (t) => {
  const sb = await sandbox(t);
  await publish(sb, '0.5.8');
  await sb.setCheck({ ready: true, tools: [{ tool: 'claude', account: false, connected: false, verified: null }, { tool: 'codex', account: false, connected: false, verified: null }] });
  const result = sb.run();
  assert.equal(result.status, 4, result.out);
  assert.match(result.out, /install the official CLI yourself and sign in/);
  assert.match(result.out, /does not install those CLIs/);
});

test('setup itself failing, or setup --check failing, is installed-not-ready with the app kept', opts, async (t) => {
  const sb = await sandbox(t);
  await publish(sb, '0.5.8');
  await sb.flag('setup_fail');
  let result = sb.run();
  assert.equal(result.status, 4, result.out);
  assert.equal(resultLine(result.out), 'installed-not-ready');
  assert.match(result.out, /aam setup failed/);
  assert.equal(await sb.appVersion(), '0.5.8');
  await rm(join(sb.state, 'setup_fail'));
  await writeFile(join(sb.state, 'check.json'), 'not json');
  result = sb.run();
  assert.equal(result.status, 4, result.out);
  assert.ok(!/OJAK_INSTALL_RESULT=ready/.test(result.out));
});

test('--with-omp is an explicit opt-in: full setup, then broker/bridge/observer/error are judged separately', opts, async (t) => {
  const sb = await sandbox(t);
  await publish(sb, '0.5.8');
  await sb.setCheck({ omp: OMP_OK });
  await sb.flag('setup.json', JSON.stringify({ ready: true, omp: OMP_OK }));
  const ok = sb.run(['--with-omp']);
  assert.equal(ok.status, 0, ok.out);
  assert.equal(resultLine(ok.out), 'ready');
  assert.match(ok.out, /omp: broker=true bridge=true observer=true/);
  assert.ok((await sb.calls()).includes('0.5.8 aam setup --with-omp'));
  assert.ok(!(await sb.calls()).includes('0.5.8 aam setup'), 'plain setup is not run in addition');
});

test('--with-omp failure keeps the Claude/Codex install ready but exits 5 and names the omp problem', opts, async (t) => {
  for (const [label, omp, expectation] of [
    ['broker down', { ...OMP_OK, broker: false }, /broker=false/],
    ['bridge down', { ...OMP_OK, bridge: false }, /bridge=false/],
    ['observer stale', { ...OMP_OK, observer: false }, /observer=false/],
    ['error string', { ...OMP_OK, error: '연결 실패 (OMP_BRIDGE_FAILED)' }, /OMP_BRIDGE_FAILED/],
  ]) {
    const sb = await sandbox(t);
    await publish(sb, '0.5.8');
    await sb.setCheck({ omp });
    const result = sb.run(['--with-omp']);
    assert.equal(result.status, 5, `${label}: ${result.out}`);
    assert.equal(resultLine(result.out), 'installed-omp-failed', label);
    assert.match(result.out, expectation, label);
    assert.equal(await sb.appVersion(), '0.5.8', label);
  }
  const sb = await sandbox(t);
  await publish(sb, '0.5.8');
  // omp not installed: the result has no omp object at all.
  const absent = sb.run(['--with-omp']);
  assert.equal(absent.status, 5, absent.out);
  assert.match(absent.out, /no omp result was reported/);
  // The top-level ready flag alone never proves omp.
  assert.match(absent.out, /ready=true/);
});

test('omp detected but not requested stays disconnected and is reported as such', opts, async (t) => {
  const sb = await sandbox(t);
  await publish(sb, '0.5.8');
  await sb.setCheck({ omp: { broker: false, bridge: false, observer: false, error: null, futureLaunchesOnly: true } });
  const result = sb.run();
  assert.equal(result.status, 0, result.out);
  assert.equal(resultLine(result.out), 'ready');
  assert.match(result.out, /NOT connected \(not requested/);
  const calls = await sb.calls();
  assert.ok(!has(calls, '--with-omp') && !has(calls, 'omp-broker') && !has(calls, 'omp-bridge'));
});

test('re-running the same version is safe: no download, no stop, setup and verification still run', opts, async (t) => {
  const sb = await sandbox(t);
  await publish(sb, '0.5.8');
  assert.equal(sb.run().status, 0);
  await sb.clearCalls();
  const again = sb.run();
  assert.equal(again.status, 0, again.out);
  assert.equal(resultLine(again.out), 'ready');
  assert.match(again.out, /already installed .* and verifies/);
  const calls = await sb.calls();
  assert.ok(!calls.some((line) => line.includes('releases/download')), 'does not download again');
  assert.ok(!has(calls, 'service stop') && !has(calls, 'hdiutil'));
  assert.ok(calls.includes('0.5.8 aam setup') && calls.includes('0.5.8 aam setup --check'));
  assert.deepEqual(await sb.entries(), ['Ojak.app']);
});

test('re-running after a failed setup resumes without reinstalling files', opts, async (t) => {
  const sb = await sandbox(t);
  await publish(sb, '0.5.8');
  await sb.flag('setup_fail');
  assert.equal(sb.run().status, 4);
  await rm(join(sb.state, 'setup_fail'));
  await sb.clearCalls();
  const retry = sb.run();
  assert.equal(retry.status, 0, retry.out);
  assert.ok(!(await sb.calls()).some((line) => line.includes('releases/download')));
});

test('interrupted installation evidence is preserved rather than overwriting uncertain state', opts, async (t) => {
  const sb = await sandbox(t);
  await makeApp(join(sb.apps, '.Ojak.app.previous'), { version: '0.5.7' });
  await sb.flag('service_running');
  await mkdir(join(sb.apps, '.Ojak.app.installing', 'Contents'), { recursive: true });
  await publish(sb, '0.5.7');
  const result = sb.run();
  assert.equal(result.status, 6, result.out);
  assert.deepEqual((await sb.entries()).sort(), ['.Ojak.app.installing', 'Ojak.app']);
  assert.equal(await sb.appVersion(), '0.5.7');
  assert.ok(!has(await sb.calls(), 'service stop'));
});

test('another installer lock blocks replacement without removing its lock', opts, async (t) => {
  const sb = await sandbox(t);
  await installOld(sb, '0.5.7');
  await publish(sb, '0.5.8');
  await mkdir(join(sb.apps, '.Ojak.app.install-lock'));
  const result = sb.run();
  assert.equal(result.status, 3, result.out);
  assert.equal(await sb.appVersion(), '0.5.7');
  assert.ok((await stat(join(sb.apps, '.Ojak.app.install-lock'))).isDirectory());
  assert.ok(!has(await sb.calls(), 'service stop'));
});

test('downgrades and a second copy in the other scope are refused without changes', opts, async (t) => {
  let sb = await sandbox(t);
  await installOld(sb, '0.5.9');
  await publish(sb, '0.5.8');
  let result = sb.run();
  assert.equal(result.status, 1);
  assert.match(result.out, /Downgrades are not done automatically/);
  assert.equal(await sb.appVersion(), '0.5.9');
  sb = await sandbox(t);
  await installOld(sb, '0.5.7', { apps: sb.sysApps });
  await publish(sb, '0.5.8');
  result = sb.run();
  assert.equal(result.status, 1);
  assert.match(result.out, /already installed in .*sysapps/);
  assert.deepEqual(await sb.entries(), []);
});

test('--system installs into /Applications through sudo and still verifies before replacing', opts, async (t) => {
  const sb = await sandbox(t);
  await installOld(sb, '0.5.7', { apps: sb.sysApps });
  await publish(sb, '0.5.8');
  const result = sb.run(['--system']);
  assert.equal(result.status, 0, result.out);
  assert.equal(await sb.appVersion(sb.sysApps), '0.5.8');
  assert.ok(has(await sb.calls(), 'sudo'));
  assert.deepEqual(await sb.entries(sb.sysApps), ['Ojak.app']);
  assert.deepEqual(await sb.entries(), []);
});

test('--skip-* flags keep their explicit meaning: only the unskipped steps run and readiness is not claimed', opts, async (t) => {
  const sb = await sandbox(t);
  await installOld(sb, '0.5.8');
  await publish(sb, '0.5.8');
  const result = sb.run(['--skip-service', '--skip-shell']);
  assert.equal(result.status, 0, result.out);
  assert.equal(resultLine(result.out), 'installed-setup-skipped');
  assert.match(result.out, /readiness is not verified/);
  const calls = await sb.calls();
  assert.ok(!has(calls, 'service stop'), '--skip-service never touches the service');
  assert.ok(!has(calls, 'aam service install') && !has(calls, 'shell install'));
  assert.ok(calls.includes('0.5.8 aam integration install'));
  assert.ok(!calls.includes('0.5.8 aam setup'));
});

test('--skip-* does not bypass the busy-session refusal', opts, async (t) => {
  const sb = await sandbox(t);
  await installOld(sb, '0.5.7', { sessions: ['ACTIVE'] });
  await publish(sb, '0.5.8');
  const result = sb.run(['--skip-service', '--skip-integration', '--skip-shell']);
  assert.equal(result.status, 3, result.out);
  assert.equal(await sb.appVersion(), '0.5.7');
});

test('--skip-service cannot replace an idle app without closing admission', opts, async (t) => {
  const sb = await sandbox(t);
  await installOld(sb, '0.5.7');
  await publish(sb, '0.5.8');
  const result = sb.run(['--skip-service']);
  assert.equal(result.status, 3, result.out);
  assert.equal(await sb.appVersion(), '0.5.7');
  assert.ok(!has(await sb.calls(), 'ditto'));
});

test('a ready flag cannot hide a failed account command check', opts, async (t) => {
  const sb = await sandbox(t);
  await publish(sb, '0.5.8');
  await sb.setCheck({ tools: [{ tool: 'claude', account: true, connected: true, verified: false }] });
  const result = sb.run();
  assert.equal(result.status, 4, result.out);
});

test('--clear-quarantine is applied to the staged copy only when asked', opts, async (t) => {
  const sb = await sandbox(t);
  await publish(sb, '0.5.8');
  await writeFile(join(sb.fake, 'xattr'), '#!/bin/sh\necho "xattr $*" >> "$FAKE_STATE/calls.log"\n');
  await chmod(join(sb.fake, 'xattr'), 0o755);
  const script = (await readFile(sb.script, 'utf8')).replace('XATTR=/usr/bin/xattr', `XATTR=${sb.fake}/xattr`);
  await writeFile(sb.script, script);
  const result = sb.run(['--clear-quarantine']);
  assert.equal(result.status, 0, result.out);
  const call = (await sb.calls()).find((line) => line.startsWith('xattr'));
  assert.match(call, /-dr com\.apple\.quarantine .*\.Ojak\.app\.installing$/);
});

test('--uninstall --dry-run only previews; --uninstall rejects install-only flags', opts, async (t) => {
  const sb = await sandbox(t);
  await installOld(sb, '0.5.8');
  const dry = sb.run(['--uninstall', '--dry-run']);
  assert.equal(dry.status, 0, dry.out);
  assert.equal(resultLine(dry.out), 'dry-run');
  const calls = await sb.calls();
  assert.deepEqual(calls, ['0.5.8 aam deactivate --dry-run']);
  assert.equal(await sb.appVersion(), '0.5.8');
  assert.equal(sb.run(['--uninstall', '--version', '0.5.8']).status, 2);
});

test('secrets are not echoed: the installer never prints the environment or token-like values', opts, async (t) => {
  const sb = await sandbox(t);
  await publish(sb, '0.5.8');
  const result = sb.run([], { GITHUB_TOKEN: 'ghp_secretvalue', ANTHROPIC_API_KEY: 'sk-secretvalue' });
  assert.equal(result.status, 0, result.out);
  assert.ok(!/ghp_secretvalue|sk-secretvalue/.test(result.out));
});
