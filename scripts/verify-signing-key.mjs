// 릴리스 전 게이트: TAURI_SIGNING_PRIVATE_KEY(+PASSWORD)로 작은 파일을 서명하고, 저장소에 커밋된
// plugins.updater.pubkey로 검증한다. 비밀키가 공개키의 짝이고 암호가 맞을 때만 통과한다. 키 내용은 출력하지 않는다.
// Tauri 서명은 minisign 형식: 서명 파일 2번째 줄 = base64("Ed" + keyId(8) + sig(64)), 공개키 2번째 줄 = base64("Ed" + keyId(8) + pk(32)).
import { execFileSync } from "node:child_process";
import { mkdtempSync, readFileSync, writeFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { createHash, createPublicKey, verify } from "node:crypto";

const conf = JSON.parse(readFileSync("apps/desktop/src-tauri/tauri.conf.json", "utf8"));
const pubkeyLine = Buffer.from(conf.plugins.updater.pubkey, "base64").toString("utf8").trim().split("\n").pop();
const pub = Buffer.from(pubkeyLine, "base64");
if (pub.length !== 42 || pub.subarray(0, 2).toString() !== "Ed") throw new Error("committed pubkey is not a minisign Ed25519 key");
const pubKeyId = pub.subarray(2, 10);
const pubRaw = pub.subarray(10);

if (!process.env.TAURI_SIGNING_PRIVATE_KEY) throw new Error("TAURI_SIGNING_PRIVATE_KEY is empty");
const dir = mkdtempSync(join(tmpdir(), "ojak-sign-"));
try {
  const file = join(dir, "probe.txt");
  writeFileSync(file, `ojak signing probe ${Date.now()}\n`);
  // 키는 인수가 아니라 임시 파일로 넘긴다. 실패 시 명령줄에 키가 찍히지 않게 오류 메시지도 바꾼다.
  const keyFile = join(dir, "key");
  writeFileSync(keyFile, process.env.TAURI_SIGNING_PRIVATE_KEY, { mode: 0o600 });
  // Windows의 npx는 .cmd라 execFileSync로 못 띄운다. 셸 없이 로컬 @tauri-apps/cli 진입 파일을 node로 직접 실행한다.
  const tauriCli = fileURLToPath(import.meta.resolve("@tauri-apps/cli/tauri.js"));
  // `tauri signer sign`은 암호를 -p로만 받는다. 명령줄·예외 메시지는 출력하지 않는다(아래 catch가 고정 문구만 낸다).
  const args = [tauriCli, "signer", "sign", "-f", keyFile];
  // 비어 있어도 -p를 넘겨야 signer가 터미널 입력을 기다리지 않는다(stdin은 닫혀 있다).
  args.push("-p", process.env.TAURI_SIGNING_PRIVATE_KEY_PASSWORD ?? "");
  args.push(file);
  // 환경변수 TAURI_SIGNING_PRIVATE_KEY가 같이 있으면 -f와 충돌해 사용법 오류가 나므로 자식에서는 뺀다.
  const { TAURI_SIGNING_PRIVATE_KEY: _key, TAURI_SIGNING_PRIVATE_KEY_PASSWORD: _pw, ...env } = process.env;
  try {
    execFileSync(process.execPath, args, { stdio: ["ignore", "ignore", "pipe"], env });
  } catch {
    throw new Error("tauri signer sign failed: wrong TAURI_SIGNING_PRIVATE_KEY_PASSWORD or malformed TAURI_SIGNING_PRIVATE_KEY");
  }
  // .sig 파일은 minisign 텍스트 전체를 다시 base64로 감싼 것이다(pubkey와 같은 방식).
  const sigLine = Buffer.from(readFileSync(`${file}.sig`, "utf8").trim(), "base64").toString("utf8").trim().split("\n")[1];
  const sig = Buffer.from(sigLine, "base64");
  // "Ed" = 파일 원문 서명, "ED" = BLAKE2b-512(파일) 서명(tauri signer가 쓰는 prehashed 방식).
  const algorithm = sig.subarray(0, 2).toString();
  if (algorithm !== "Ed" && algorithm !== "ED") throw new Error("signature is not a minisign Ed25519 signature");
  if (!sig.subarray(2, 10).equals(pubKeyId)) throw new Error("signing key id does not match the committed pubkey");
  const key = createPublicKey({ key: Buffer.concat([Buffer.from("302a300506032b6570032100", "hex"), pubRaw]), format: "der", type: "spki" });
  const message = algorithm === "ED" ? createHash("blake2b512").update(readFileSync(file)).digest() : readFileSync(file);
  if (!verify(null, message, key, sig.subarray(10))) throw new Error("signature does not verify against the committed pubkey");
  console.log("signing key matches committed updater pubkey");
} finally {
  rmSync(dir, { recursive: true, force: true });
}
