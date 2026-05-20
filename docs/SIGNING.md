# Windows code-signing — Azure Trusted Signing

The PackRelay launcher's Windows installers (`.exe` + `.msi`) are
signed via [**Azure Trusted Signing**](https://learn.microsoft.com/en-us/azure/trusted-signing/),
Microsoft's hosted code-signing service. Signing happens in CI on
every tagged release via Levminer's
[`artifact-signing-cli`](https://github.com/Levminer/trusted-signing-cli)
(published as `trusted-signing-cli` through 0.9.x ~ renamed at 0.10.0;
the GitHub repo keeps its original name), invoked through Tauri's
`bundle.windows.signCommand` hook.

This document is the operator walkthrough — what to set up once
when Azure verification completes, and how to verify a signed
binary lands on a user's machine correctly.

For the *why* (Safe Browsing context, alternatives we considered),
see kanban card #173.

---

## Status checklist

- [x] `release.yml` provisions `artifact-signing-cli` on Windows runners
- [x] `tauri.conf.json` invokes `sign-windows.ps1` via the signCommand
- [x] `sign-windows.ps1` no-ops gracefully when secrets are absent
- [x] **Azure identity verification approved** (Individual / Public Trust, `Dara Hensley`)
- [x] **Certificate profile created** (`packrelay-prod`, Active)
- [x] **GitHub secrets added** (all six `AZURE_*` as repo Secrets)
- [ ] **First signed release shipped** ← cut a tag, watch the workflow, confirm signature

---

## One-time setup (do once verification approves)

> **Naming note:** Microsoft now brands this service "Azure
> Artifact Signing" (formerly "Trusted Signing"). The Azure portal
> + the resource type label say "Artifact Signing", but the ARM
> provider namespace is unchanged: `Microsoft.CodeSigning`. Don't
> let the rebrand confuse you ~ they're the same service.

### 1. Create a Certificate Profile in Azure

In the Azure portal, navigate to your Artifact Signing Account
(`packrelay`, in resource group `pack-relay-signing`) →
**Certificate Profiles** → **+ New certificate profile**.

- **Certificate profile name:** `packrelay-prod` (this is what
  `AZURE_CERT_PROFILE_NAME` below points at)
- **Identity validation:** select the verified Public Trust identity
  you submitted earlier
- **Certificate type:** **Public Trust** — the SmartScreen-visible
  variant
- **Include `quotes` in the cert:** leave as default

Click **Create**. Provisioning takes ~30 sec. From this moment
forward you're billing at the Basic tier rate (~$9.99/mo).

### 2. Create a Service Principal for CI

GitHub Actions needs a non-interactive Azure identity that can
request signatures on your behalf. Use the Azure CLI (or the
portal — both work, CLI is faster):

```bash
# Sign in interactively first if not already
az login

# Replace <subscription-id> with your real subscription ID
# (Azure portal → Subscriptions, or `az account show --query id -o tsv`).
az ad sp create-for-rbac \
  --name packrelay-launcher-signing \
  --role "Trusted Signing Certificate Profile Signer" \
  --scopes "/subscriptions/<subscription-id>/resourceGroups/pack-relay-signing/providers/Microsoft.CodeSigning/codesigningaccounts/packrelay"
```

> The scope path above uses the **real** resource group
> (`pack-relay-signing`) and account name (`packrelay`) from the
> deployment. If the portal shows the RBAC role under a different
> name post-rebrand, pick the one whose description is "sign with
> a certificate profile" — the role definition itself is unchanged.

The CLI emits a JSON blob with three values you need:

```json
{
  "appId": "AAAAAAAA-AAAA-AAAA-AAAA-AAAAAAAAAAAA",     // -> AZURE_CLIENT_ID
  "displayName": "packrelay-launcher-signing",
  "password": "secret-string-rotates-every-2-years",  // -> AZURE_CLIENT_SECRET
  "tenant": "BBBBBBBB-BBBB-BBBB-BBBB-BBBBBBBBBBBB"     // -> AZURE_TENANT_ID
}
```

**Save the `password` immediately** — Azure will never show it again.
If you lose it, you'll need to generate a new client secret.

### 3. Add the secrets to this GitHub repo

GitHub repo → **Settings** → **Secrets and variables** → **Actions**
→ **Secrets** tab.

All six values go in as **Repository secrets**. The last three
aren't sensitive (an endpoint URL, two resource names), but
keeping everything as Secrets ~ rather than splitting three into
the Variables tab ~ avoids the `${{ vars.* }}` empty-string trap
if only one tab ends up populated. `release.yml` reads all six
via `${{ secrets.* }}`.

| Name | Value |
|---|---|
| `AZURE_TENANT_ID` | the `tenant` from step 2 |
| `AZURE_CLIENT_ID` | the `appId` from step 2 |
| `AZURE_CLIENT_SECRET` | the `password` from step 2 |
| `AZURE_ENDPOINT` | `https://eus.codesigning.azure.net` *(East US — the account is deployed in `eastus`, so this is correct as-is)* |
| `AZURE_CODE_SIGNING_ACCOUNT_NAME` | `packrelay` *(the deployed Artifact Signing Account name)* |
| `AZURE_CERT_PROFILE_NAME` | `packrelay-prod` *(matches what you created in step 1)* |

### 4. Cut a release to verify

```bash
git tag v0.1.9
git push --tags
```

Watch the release workflow run. The Windows job should show
`[sign-windows] Signing ... OK` in the build logs for each `.exe`
and `.msi` artifact.

### 5. Verify a signed binary

Download the freshly signed `PackRelay_0.1.9_x64-setup.exe` from
the GitHub release. From a Windows machine:

```powershell
Get-AuthenticodeSignature .\PackRelay_0.1.9_x64-setup.exe |
  Select-Object SignerCertificate, Status, TimeStamperCertificate
```

Expected output:

```
SignerCertificate     : [Subject: CN=<your verified legal name>, ...]
Status                : Valid
TimeStamperCertificate: [Subject: ..., O=Microsoft, ...]
```

`Status = Valid` is the win condition. If you see `NotSigned` or
`HashMismatch`, the workflow's sign step didn't run or didn't
complete — check Actions logs.

---

## Operational notes

### Credential rotation

`AZURE_CLIENT_SECRET` expires every **2 years** by default. Set a
calendar reminder. To rotate:

```bash
az ad app credential reset --id <AZURE_CLIENT_ID>
```

Save the new `password`, update the `AZURE_CLIENT_SECRET` GitHub
secret. The cert and identity itself don't rotate — only the
service principal's auth secret does.

### Local dev / forks / PR builds

`sign-windows.ps1` checks `$env:AZURE_CLIENT_SECRET` and skips
signing if it's not set. So:

- Local Windows dev builds: unsigned, no error
- PRs from forks: unsigned, no error (forks don't have access to
  upstream secrets — GitHub's standard protection)
- Untagged pushes to main: unsigned (workflow only runs on tags)

This is intentional. The single source of signed binaries is
tagged releases from this upstream repo, built on GitHub Actions.

### Cost monitoring

The Basic tier is **$9.99/mo flat + 5000 free signatures included
per month**. With ~10 signatures per tagged release (5 artifacts ×
~2 signing operations each due to NSIS bundling), you'd need ~500
releases per month to exceed the included quota. You will not
exceed it.

To check your usage: Azure portal → Trusted Signing Account →
**Metrics** → "Total signatures."

### If signing fails in CI

The workflow surfaces non-zero exit codes from `sign-windows.ps1`
loud and clear. Common failure modes:

| Symptom | Cause | Fix |
|---|---|---|
| `AZURE_CLIENT_SECRET not set - skipping signing` in logs | Secrets not configured | Add them per step 3 |
| `Required env var 'AZURE_ENDPOINT' is not set` | Secret missing | Add it as a repo Secret per step 3 |
| `artifact-signing-cli: command not found` | Cargo install step skipped | Check `if: matrix.platform == 'windows-latest'` ran |
| `403 Forbidden` from Azure | Service principal lacks the Signer role | Re-run `az ad sp create-for-rbac` with correct `--role` scope |
| `Certificate profile 'foo' not found` | `AZURE_CERT_PROFILE_NAME` typo | Match the name from Azure portal exactly |

### Disabling signing temporarily

Delete the `AZURE_CLIENT_SECRET` secret. The script will then
short-circuit on every Windows artifact and the build will
succeed with unsigned binaries. Easier than reverting the workflow.

---

## See also

- Microsoft docs: [Trusted Signing concepts](https://learn.microsoft.com/en-us/azure/trusted-signing/concept-trusted-signing-resources-roles)
- Tauri docs: [Windows code-signing](https://v2.tauri.app/distribute/sign/windows/)
- Card #173 on the packrelay-raunk kanban for the original design notes
- `docs/NOTARIZATION.md` for the parallel macOS signing flow
