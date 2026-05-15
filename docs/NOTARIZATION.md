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

> ⚠️ **Cert-type pitfall.** Apple's certificate dropdown also
> offers **"Apple Development"** and **"Apple Distribution"** —
> those are *different products* despite the similar names:
>
> | Cert type | Used for | What we want? |
> |---|---|---|
> | **Apple Development** | Running your app on YOUR dev devices via Xcode. | ❌ no |
> | **Apple Distribution** | Submitting to the Mac App Store. | ❌ no |
> | **Developer ID Application** | Distributing OUTSIDE the App Store (.dmg, direct download). | ✅ yes — pick this one |
> | **Developer ID Installer** | Same but for .pkg installers. We don't ship .pkg. | ❌ no |
>
> If you accidentally pick "Apple Development" and export THAT
> as your .p12, the CI build will fail with
> `certificate ... does not match provided identity` — because
> tauri-action looks for a cert whose Common Name starts with
> "Developer ID Application:" and the import gave it a cert
> starting with "Apple Development:". See the troubleshooting
> section below.

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

> 🔑 **The Team ID is also literally in the cert name** — the
> 10-character string in parentheses. Whatever value lands in
> `APPLE_TEAM_ID` (step 2) MUST be the same Team ID that
> appears in this string. If your Apple ID is on multiple teams
> (personal + org, agency clients, etc.) it's easy to grab the
> wrong one. When in doubt, copy the parenthesized ID from this
> output verbatim — that's the source of truth for which team's
> cert you actually exported.

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

**`certificate from APPLE_CERTIFICATE "Apple Development: <Name> (<TEAMID>)" environment variable does not match provided identity "***"`**

The .p12 you uploaded contains the *wrong* type of certificate.
tauri-action imports whatever .p12 is in `APPLE_CERTIFICATE`,
then asks codesign to find a cert whose Common Name matches
`APPLE_SIGNING_IDENTITY` (which we set to start with
`Developer ID Application:`). The error means the import gave
it an "Apple Development:" cert instead — that's a dev/testing
cert, not a distribution cert.

**Fix:** Go back to step 1a and create a **Developer ID
Application** certificate. The Apple Developer dashboard offers
several cert types in the "+ Certificate" picker; only that
specific one works for notarized .dmg distribution. See the
table in step 1a for the full breakdown.

Once the new cert is in Keychain, re-export to `.p12`, re-base64,
and overwrite `APPLE_CERTIFICATE` + `APPLE_CERTIFICATE_PASSWORD`.

**Same error but with `"Developer ID Application:"` on both sides
(cert name and identity)**

The cert IS the right type, but the Team IDs disagree. The
parenthesized 10-char string in the cert's Common Name must
match `APPLE_TEAM_ID` exactly. This happens when:

- Your Apple ID is on multiple Developer teams (personal +
  agency, for example) and `APPLE_TEAM_ID` was set from the
  wrong team.
- A previous cert from a different team is still in Keychain
  and got picked up.

**Fix:** Run `security find-identity -v -p codesigning` on
your Mac and copy the parenthesized Team ID directly out of
the cert label. Use *that* value for `APPLE_TEAM_ID` (and
make sure `APPLE_SIGNING_IDENTITY` is the full string from
that same line).

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

**Notary submit hangs for 30+ minutes**
Apple's notary service runs on a shared queue and can occasionally
back up — submissions that normally finish in 5–15 minutes sometimes
sit for an hour or more during heavy load (often correlated with
new Xcode releases or WWDC week). Nothing's broken on our side; the
job is literally waiting for `notarytool submit --wait` to return.

The GitHub job timeout is 6 hours so you don't need to babysit it.
If a run gets stuck and you want a green release without macOS:
cancel just the macOS leg, the Linux + Windows artifacts are
already uploaded.

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
