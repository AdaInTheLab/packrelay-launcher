# macOS notarization setup

This doc walks through the one-time setup for notarized macOS
builds. Like the [updater keypair](./UPDATER.md), you only do
this once, ever. After that every tagged release auto-notarizes.

Once notarization is wired, the "right-click → Open" warning
goes away — `.dmg` users can double-click like any other app.

## Background

Notarization is Apple's "we've scanned this binary and didn't
find anything malicious" stamp. It's not the same as code-signing
(which proves the binary came from a specific developer); both
are needed for a frictionless install. Tauri's macOS bundler
handles both in one shot when the right env vars are set.

Apple's flow has two halves:

- **Code signing**: a Developer ID Application certificate from
  your Apple Developer account. We export it as a `.p12` and
  paste the base64 into a GitHub secret.
- **Notarization**: Apple's notary service checks the signed
  bundle and returns a "ticket" we then "staple" into the .dmg.
  Authenticated via your Apple ID + an app-specific password +
  your Team ID.

## 1 — Get the Developer ID Application certificate

You need to be enrolled in the Apple Developer Program
($99/year). Doc assumes that's done.

### 1a — Create the certificate

1. Open **Keychain Access** on your Mac.
2. Menu bar → **Keychain Access** → **Certificate Assistant** →
   **Request a Certificate From a Certificate Authority...**
3. Fill in your Apple ID email + your name.
4. **CA Email Address**: leave blank.
5. **Request is**: **Saved to disk** (not "Email to CA").
6. Save the `.certSigningRequest` file somewhere.

7. Go to https://developer.apple.com/account/resources/certificates/list
8. Click the **+** button to create a new certificate.
9. Choose **Developer ID Application** (NOT "Developer ID
   Installer" — that's for .pkg, we need the one for .app).
10. Upload the `.certSigningRequest` file.
11. Download the resulting `.cer` file.
12. Double-click the `.cer` to install it into Keychain Access.

### 1b — Export to .p12

1. In Keychain Access, find the new certificate (it'll be named
   `Developer ID Application: Your Name (TEAMID)`).
2. **Expand the certificate** to show its private key beneath.
   Select BOTH the cert AND the private key (Cmd-click).
3. Right-click → **Export 2 items...**
4. Save as `developer-id-application.p12`.
5. Set a password — pick something strong, store it (we'll
   paste it into a GitHub secret).

### 1c — Note your signing identity string

The full string is `Developer ID Application: Your Name (TEAMID)`
— this is the certificate's Common Name. You can find it by:

```bash
security find-identity -v -p codesigning
```

Look for the line that starts with `Developer ID Application:`.
Copy the WHOLE quoted name, including the `(TEAMID)` suffix.

## 2 — Get your Team ID

Go to https://developer.apple.com/account → **Membership details**.
Your Team ID is the 10-character alphanumeric string. It also
appears in parentheses in the signing-identity string above.

## 3 — Create an app-specific password

1. Sign in to https://appleid.apple.com.
2. Scroll to **Sign-In and Security** → **App-Specific Passwords**.
3. Click **Generate an app-specific password** (or the **+** icon).
4. Label it something like "PackRelay CI notarization".
5. Apple shows the password ONCE — copy it now. Format: `xxxx-xxxx-xxxx-xxxx`.

This isn't your regular Apple ID password. It's a per-app token
that bypasses 2FA when CI calls Apple's notary service.

## 4 — Base64-encode the .p12

GitHub secrets only handle text, so we base64-encode the
binary cert. On your Mac:

```bash
base64 -i developer-id-application.p12 | pbcopy
```

That copies the base64 to your clipboard. (Or `> cert.b64.txt`
to write to a file you can open and copy from.)

## 5 — Paste the six GitHub secrets

Repo → Settings → Secrets and variables → Actions → New
repository secret. Add all six:

| Secret name | Value |
|---|---|
| `APPLE_CERTIFICATE` | the base64 string from step 4 |
| `APPLE_CERTIFICATE_PASSWORD` | the password from step 1b |
| `APPLE_SIGNING_IDENTITY` | the full `Developer ID Application: ...` string from step 1c |
| `APPLE_ID` | your Apple ID email |
| `APPLE_PASSWORD` | the app-specific password from step 3 (with hyphens) |
| `APPLE_TEAM_ID` | the 10-char Team ID from step 2 |

The release workflow (`.github/workflows/release.yml`) already
references these by exactly those names — tauri-action picks
them up automatically once they're set.

## 6 — Tag a release to test

Bump the version in `app/src-tauri/tauri.conf.json` + `app/package.json`
+ root `Cargo.toml`, commit, tag, push:

```bash
git tag v0.1.4
git push origin v0.1.4
```

After the workflow runs, the macOS job's log should include:

- `signing the app bundle with identity Developer ID Application: ...`
- `submitting to Apple's notary service...`
- `notarization status: Accepted`
- `stapling ticket to ...PackRelay.dmg`

If you see those four lines, notarization worked. The published
`.dmg` and `.app.tar.gz` are now Gatekeeper-trusted — users can
double-click instead of right-click-Open.

## Troubleshooting

**"unable to build chain to self-signed root for signer"**
Your Developer ID cert isn't trusted by the macOS Apple Worldwide
Developer Relations intermediate cert. tauri-action installs the
chain automatically; if it fails, re-download the intermediate
from https://www.apple.com/certificateauthority/ and re-import.

**"notarization status: Invalid"**
The notary service rejected the submission. Look at the JSON
response in the log for the actual reason. Most common:
- `hardened runtime missing` — Tauri sets this by default in v2,
  but a custom bundle config might unset it.
- `signed with deprecated key` — your cert chain points at an
  old intermediate. Re-create the cert.

**"asynchronously stapling"**
Stapling can take ~30s. Not a failure unless followed by an error.

**"You are not a member of any teams"**
APPLE_TEAM_ID is wrong, or the Apple ID isn't actually enrolled
in the Developer Program. Double-check the Team ID at
developer.apple.com/account → Membership details.

## Rotation

Apple Developer ID certs expire every 5 years. To rotate:

1. Repeat steps 1a–1c with the new expiry.
2. Update `APPLE_CERTIFICATE` + `APPLE_CERTIFICATE_PASSWORD` +
   `APPLE_SIGNING_IDENTITY` in GitHub secrets.
3. Ship a new release.

The app-specific password is independent — it doesn't expire
unless you delete it at appleid.apple.com. If you ever do:
regenerate at step 3 and update `APPLE_PASSWORD`.
