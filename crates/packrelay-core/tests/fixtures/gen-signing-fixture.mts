/**
 * Generates signing-fixture.json: manifests signed exactly the way the
 * cloud signs and stores them, plus canonical-JSON cross-check cases,
 * all produced by PackRelayCloud's own code. The Rust tests in
 * packrelay-core check the launcher's canonicalizer + verifier against
 * this output, so the two implementations can't drift apart silently.
 *
 * Run from a PackRelayCloud checkout (its tsconfig resolves the "@/"
 * imports inside the cloud modules):
 *
 *   cd <PackRelayCloud>
 *   npx tsx <launcher>/crates/packrelay-core/tests/fixtures/gen-signing-fixture.mts
 *
 * Keys come from fixed seeds, so re-running produces the same file
 * unless the cloud's canonicalization or schema changes.
 */
import { createHash, createPrivateKey, createPublicKey, sign } from "node:crypto";
import { execSync } from "node:child_process";
import { writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const cloudDir = resolve(process.env.PACKRELAY_CLOUD_DIR ?? process.cwd());
const here = dirname(fileURLToPath(import.meta.url));

const { canonicalize, canonicalBytes } = await import(
  pathToFileURL(join(cloudDir, "src/lib/canonical-json.ts")).href
);
const { parseManifest, verifyManifestSignature } = await import(
  pathToFileURL(join(cloudDir, "src/lib/manifest.ts")).href
);

const KEY_ID = "fixture-pub/main";

// Ed25519 key from a fixed 32-byte seed, via Node's built-in crypto
// (PKCS#8 wrapper around the raw seed). Signing with node:crypto and
// verifying with the cloud's @noble/ed25519 path keeps this honest.
function keyFromSeed(fill: number) {
  const seed = Buffer.alloc(32, fill);
  const pkcs8 = Buffer.concat([
    Buffer.from("302e020100300506032b657004220420", "hex"),
    seed,
  ]);
  const privateKey = createPrivateKey({ key: pkcs8, format: "der", type: "pkcs8" });
  const jwk = createPublicKey(privateKey).export({ format: "jwk" });
  const publicKey = Buffer.from(jwk.x as string, "base64url").toString("base64");
  return { privateKey, publicKey };
}

const signer = keyFromSeed(0x11);
const stranger = keyFromSeed(0x22);
// A second key the same publisher "rotates" to, for key-pinning tests.
const rotated = keyFromSeed(0x33);
const ROTATED_KEY_ID = "fixture-pub/rotated";

type KeyPair = ReturnType<typeof keyFromSeed>;

function signManifest(unsigned: Record<string, unknown>, key: KeyPair = signer, keyId = KEY_ID) {
  const value = sign(null, canonicalBytes(unsigned), key.privateKey).toString("hex");
  return { ...unsigned, signature: { algo: "ed25519", publicKeyId: keyId, value } };
}

// What the versions route stores and the manifest route serves:
// JSON.stringify of the Zod-parsed manifest. Verified with the cloud's
// own verifier so a fixture the cloud would reject can't sneak in.
async function stored(signed: Record<string, unknown>, key: KeyPair = signer, wrongKey: KeyPair = stranger) {
  const parsed = parseManifest(signed);
  if (!parsed.ok) throw new Error(`cloud rejected fixture manifest: ${parsed.error}`);
  if (!(await verifyManifestSignature(parsed.manifest, key.publicKey))) {
    throw new Error("cloud failed to verify fixture signature");
  }
  if (await verifyManifestSignature(parsed.manifest, wrongKey.publicKey)) {
    throw new Error("cloud verified fixture against the wrong key");
  }
  return JSON.stringify(parsed.manifest);
}

const sha = (c: string) => c.repeat(64);

const v2 = signManifest({
  schemaVersion: 2,
  name: "fixture-pack",
  displayName: "Fixture Pack — «quotes» \"and\" \\ back\\slashes",
  version: "1.2.3-beta.1",
  game: "7d2d",
  gameVersion: "V 2.4 (b7)",
  publisher: "Fixture Publisher",
  publishedAt: "2026-09-30T12:00:00.000Z",
  description: "Line one\nLine\ttwo\r\n\u0001ctrl \u001f unit \u007f del \u2028 sep / slash \u{1f98a} fox é",
  tags: ["overhaul", "ünïcödé"],
  sources: [
    { id: "nexus-42", source: "nexus", game: "7daystodie", modId: 42, fileId: 1001, version: "2.4.0" },
    {
      id: "gh-kitsune",
      source: "github",
      owner: "AdaInTheLab",
      repo: "kitsune.mod",
      releaseTag: "v1.0.0",
      assetName: "kitsune.zip",
      sha256: sha("a"),
      discoveredVia: "7dtm",
      scanAttestation: { scannedAt: "2026-09-01T00:00:00Z", scanner: "Norton 360" },
    },
    {
      id: "sevendtm-thing",
      source: "7dtm",
      modSlug: "some-thing",
      version: "3",
      upstreamUrl: "https://example.com/some-thing.zip",
    },
    { id: "legacy", source: "legacy-blob" },
  ],
  files: [
    { path: "Mods/Kitsune/ModInfo.xml", sha256: sha("1"), size: 512, sourceRef: "gh-kitsune" },
    { path: "Mods/Kitsune/Kitsune.dll", sha256: sha("2"), size: 4294967296, executable: true, sourceRef: "gh-kitsune" },
    { path: "Mods/Nexus/Config/blocks.xml", sha256: sha("3"), size: 0, sourceRef: "nexus-42" },
    { path: "Mods/Loose/readme.txt", sha256: sha("4"), size: 7 },
  ],
});

const v1 = signManifest({
  schemaVersion: 1,
  name: "fixture-legacy",
  displayName: "Fixture Legacy",
  version: "0.1.0",
  game: "7d2d",
  gameVersion: "1.0",
  publisher: "Fixture Publisher",
  publishedAt: "2026-01-01T00:00:00Z",
  files: [{ path: "Mods/Old/ModInfo.xml", sha256: sha("5"), size: 99 }],
});

// Validly signed, but for a game neither the cloud nor the launcher
// knows. parseManifest would reject it, so it's stored as-is.
const otherGame = signManifest({
  schemaVersion: 3,
  name: "fixture-minecraft",
  displayName: "Fixture Minecraft",
  version: "1.0.0",
  game: "minecraft",
  gameVersion: "1.21",
  publisher: "Fixture Publisher",
  publishedAt: "2026-09-30T12:00:00Z",
  files: [{ path: "mods/x.jar", sha256: sha("6"), size: 10 }],
});

// Validly signed, v2, claiming Valheim: v1 and v2 are 7DTD-only, so
// the cloud rejects it and it's stored as-is.
const v2Valheim = signManifest({
  schemaVersion: 2,
  name: "fixture-v2-valheim",
  displayName: "Fixture v2 Valheim",
  version: "1.0.0",
  game: "valheim",
  gameVersion: "0.220.5",
  publisher: "Fixture Publisher",
  publishedAt: "2026-09-30T12:00:00Z",
  files: [{ path: "plugins/x.dll", sha256: sha("6"), size: 10 }],
});

// A v3 Valheim pack exactly as the cloud accepts it: a Thunderstore
// source, BepInEx as its framework, paths relative to BepInEx/.
const v3Valheim = signManifest({
  schemaVersion: 3,
  name: "fixture-valheim",
  displayName: "Fixture Valheim",
  version: "1.0.0",
  game: "valheim",
  gameVersion: "0.220.5",
  framework: { id: "bepinexpack-valheim", version: "5.4.2333" },
  publisher: "Fixture Publisher",
  publishedAt: "2026-09-30T12:00:00Z",
  sources: [
    {
      id: "ts-jotunn",
      source: "thunderstore",
      community: "valheim",
      namespace: "ValheimModding",
      name: "Jotunn",
      version: "2.29.2",
      sha256: sha("b"),
    },
  ],
  files: [
    { path: "plugins/ValheimModding-Jotunn/Jotunn.dll", sha256: sha("7"), size: 796116, sourceRef: "ts-jotunn" },
    { path: "config/com.jotunn.jotunn.cfg", sha256: sha("8"), size: 120, sourceRef: "ts-jotunn" },
  ],
});

// Canonical-JSON cross-check cases. Keys cover UTF-16 vs code-point
// ordering (U+1F98A sorts before U+E000 in UTF-16, after it by code
// point); strings cover every escape JSON.stringify makes.
const controlChars = Array.from({ length: 0x20 }, (_, i) => String.fromCharCode(i)).join("");
const canonicalInputs: unknown[] = [
  null,
  true,
  false,
  0,
  -1,
  9007199254740991,
  -9007199254740991,
  4294967296,
  "",
  controlChars + "\u007f\u0080\u2028\u2029/\\\"'<>&",
  "🦊 é ü 中文",
  [],
  {},
  [1, "a", null, [true, {}], { z: 1, a: [] }],
  { b: 1, a: 2, B: 3, _: 4, "10": 5, "9": 6, "": 7 },
  { "\ue000": "private-use", "\u{1f98a}": "astral", "\uffff": "bmp-max", "é": "latin", z: "ascii" },
  { outer: { zeta: [{ y: 2, x: 1 }], alpha: { "b c": "d\ne" } } },
];

// End-to-end pack for install/update tests (key pinning). Its files
// are real, so the launcher can download and hash-check them. v1.1.0
// exists twice: signed with the pack's usual key, and with a rotated
// one, as if the publisher had changed keys between versions.
const e2eFiles: Record<string, string> = {
  "Mods/E2E/hello.txt": "hello from the fixture pack\n",
  "Mods/E2E/second.txt": "added in 1.1.0\n",
};
const e2eEntry = (path: string) => ({
  path,
  sha256: createHash("sha256").update(e2eFiles[path]).digest("hex"),
  size: Buffer.byteLength(e2eFiles[path]),
});
const e2eManifest = (version: string, paths: string[]) => ({
  schemaVersion: 2,
  name: "fixture-e2e",
  displayName: "Fixture E2E",
  version,
  game: "7d2d",
  gameVersion: "V 2.4",
  publisher: "Fixture Publisher",
  publishedAt: "2026-09-30T12:00:00Z",
  files: paths.map(e2eEntry),
});
const e2eV100 = e2eManifest("1.0.0", ["Mods/E2E/hello.txt"]);
const e2eV110 = e2eManifest("1.1.0", ["Mods/E2E/hello.txt", "Mods/E2E/second.txt"]);

let cloudSha = "unknown";
try {
  cloudSha = execSync("git rev-parse --short HEAD", { cwd: cloudDir }).toString().trim();
} catch {}

const fixture = {
  generatedBy: "crates/packrelay-core/tests/fixtures/gen-signing-fixture.mts",
  cloudCommit: cloudSha,
  keyId: KEY_ID,
  publicKey: signer.publicKey,
  wrongPublicKey: stranger.publicKey,
  manifests: {
    v2: await stored(v2),
    v1: await stored(v1),
    otherGame: JSON.stringify(otherGame),
    v2Valheim: JSON.stringify(v2Valheim),
    v3Valheim: await stored(v3Valheim),
  },
  canonical: canonicalInputs.map((input) => ({ input, canonical: canonicalize(input) })),
  e2e: {
    rotatedKeyId: ROTATED_KEY_ID,
    rotatedPublicKey: rotated.publicKey,
    // sha256 -> file contents, served by the test's fake file endpoint.
    files: Object.fromEntries(
      Object.keys(e2eFiles).map((p) => [e2eEntry(p).sha256, e2eFiles[p]])
    ),
    manifests: {
      v100: await stored(signManifest(e2eV100)),
      v110: await stored(signManifest(e2eV110)),
      v110Rotated: await stored(signManifest(e2eV110, rotated, ROTATED_KEY_ID), rotated, signer),
    },
  },
};

const out = join(here, "signing-fixture.json");
writeFileSync(out, JSON.stringify(fixture, null, 2) + "\n");
console.log(`wrote ${out} (cloud ${cloudSha})`);
