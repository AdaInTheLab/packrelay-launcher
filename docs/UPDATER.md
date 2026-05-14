# Auto-update setup

The launcher uses [tauri-plugin-updater](https://github.com/tauri-apps/plugins-workspace/tree/v2/plugins/updater)
to check `https://github.com/AdaInTheLab/packrelay-launcher/releases/latest/download/latest.json`
on launch and prompt the user when a new version is available.
Signature verification happens locally against a public key
baked into the launcher, so even a fully-compromised release URL
can't push a malicious binary without the private signing key.

This doc walks through the one-time setup. You only need to do
this **once**, ever. After that, every tagged release on GitHub
ships a signed update that existing installs can pick up.

## 1 — Generate the signing keypair (one-time, local)

From the repo root:

```bash
cd app
npx @tauri-apps/cli signer generate -w ../tauri-update.key
```

That writes:

- `tauri-update.key` — the **private** key. Treat like an SSH key.
  **Do not commit this file.** Add `tauri-update.key` to
  `.gitignore` if it isn't already.
- `tauri-update.key.pub` — the **public** key. This is safe to
  commit and goes into `tauri.conf.json`.

The command also prompts for a password. Pick a strong one and
store it somewhere durable (1Password, Bitwarden, etc) — you'll
need it for the CI secret in step 3.

## 2 — Replace the public-key placeholder

Open `app/src-tauri/tauri.conf.json`. Find:

```json
"plugins": {
  "updater": {
    "active": true,
    "pubkey": "REPLACE_ME_AFTER_GENERATING_KEYPAIR",
    ...
  }
}
```

Replace the value with the contents of `tauri-update.key.pub`
(a long single-line base64-ish string). Commit + push that
change.

## 3 — Set the GitHub Actions secrets

In the repo on github.com → Settings → Secrets and variables →
Actions → New repository secret. Add two:

- `TAURI_SIGNING_PRIVATE_KEY` — the **full contents** of
  `tauri-update.key` (the private key, including the
  `untrusted comment:` header line and the base64 blob).
- `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` — the password you
  picked in step 1.

`.github/workflows/release.yml` already passes both through to
`tauri-action` as env vars; once they're set, the next tag push
will produce signed bundles.

## 4 — Tag the first signed release

Bump `app/src-tauri/tauri.conf.json` `version` (and
`app/package.json` if you keep them in lockstep), commit, tag,
and push:

```bash
git tag v0.2.0
git push --tags
```

After the workflow finishes, the release page should include
`latest.json` alongside the .exe / .dmg / .AppImage bundles.
That's what the in-launcher updater fetches.

## 5 — Verify the loop

1. Install the **previous** release on a test machine.
2. Tag a new release with a bumped version.
3. Launch the installed app — within a few seconds the
   "Update vX.Y.Z available" toast appears bottom-right.
4. Click **Install**. The download progress bar fills, then
   "Restarting…" pill, then the app relaunches at the new
   version.

If nothing happens:
- Check the dev console (the launcher logs `[updater] check failed:`
  for any error).
- Verify `latest.json` actually attached to the release on GitHub.
- Confirm the public key in `tauri.conf.json` matches the keypair
  whose private half is in the CI secrets.

## Rotation / loss

The private key is the single point of failure for trustworthy
updates. If it leaks:

1. Generate a new keypair (step 1) with a different filename.
2. Update the public key in `tauri.conf.json`.
3. Replace the GitHub secrets with the new private key + password.
4. Ship a new release.

Users on the **previous** version will stop receiving updates
through the in-app flow (their bundled public key doesn't match
the new private key). They'll need to download the new build
manually from the website / GitHub releases page once. After
that, all future updates flow normally.

## Why static `latest.json` instead of a Cloud API

We could host the manifest endpoint on packrelay.cloud and gain
channels (beta / stable), staged rollouts, etc. v0 doesn't need
any of that — the latest GitHub release as the source of truth
is the cheapest design that ships a real auto-updater. Cutover
later is one config change in `tauri.conf.json` plus a Cloud
route handler.
