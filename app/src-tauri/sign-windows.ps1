# Invoked by Tauri's `bundle.windows.signCommand` on each Windows
# artifact (NSIS .exe, .msi). Wraps `trusted-signing-cli` so that:
#
#   * CI with Azure secrets   -> sign with Azure Trusted Signing
#   * Local dev (no secrets)  -> skip signing, exit 0, build still wins
#
# This lets the same tauri.conf.json work for everyone without
# branching builds. The bundler calls this script for every Windows
# artifact path it produces; we pass it through to the CLI when we
# have credentials, otherwise we no-op.
#
# Required env vars when signing is desired (set in GitHub Actions
# via repo secrets / variables):
#   AZURE_CLIENT_ID                 - Service Principal app ID
#   AZURE_CLIENT_SECRET             - Service Principal secret
#   AZURE_TENANT_ID                 - Entra tenant ID
#   AZURE_ENDPOINT                  - e.g. https://eus.codesigning.azure.net
#   AZURE_CODE_SIGNING_ACCOUNT_NAME - Trusted Signing Account resource name
#   AZURE_CERT_PROFILE_NAME         - Certificate Profile name
#
# Authentication: `trusted-signing-cli` uses Azure's
# DefaultAzureCredential under the hood, which reads
# AZURE_CLIENT_ID / AZURE_CLIENT_SECRET / AZURE_TENANT_ID
# automatically. We don't pass them on the command line.

param(
    [Parameter(Mandatory = $true, Position = 0)]
    [string]$ArtifactPath
)

$ErrorActionPreference = "Stop"

# If creds aren't present, signing is intentionally skipped. This
# is the path local devs and PR-builds take. The artifact stays
# unsigned but the build itself succeeds.
if (-not $env:AZURE_CLIENT_SECRET) {
    Write-Host "[sign-windows] AZURE_CLIENT_SECRET not set - skipping signing for $ArtifactPath"
    exit 0
}

# Required-when-signing config values. We hard-fail if signing was
# attempted but config is missing - silent unsigned artifacts in
# CI would be worse than a loud failure.
$required = @(
    "AZURE_TENANT_ID",
    "AZURE_CLIENT_ID",
    "AZURE_ENDPOINT",
    "AZURE_CODE_SIGNING_ACCOUNT_NAME",
    "AZURE_CERT_PROFILE_NAME"
)
foreach ($var in $required) {
    if (-not (Get-Item "env:$var" -ErrorAction SilentlyContinue)) {
        Write-Error "[sign-windows] Required env var '$var' is not set."
        exit 1
    }
}

Write-Host "[sign-windows] Signing $ArtifactPath ..."

# trusted-signing-cli wraps signtool.exe + the Microsoft signing
# dlib. Maintained by Levminer; the canonical community CLI for
# this exact use case in the Tauri ecosystem.
#
# Flag names verified against the pinned v0.5.0 (clap arg defs in
# src/main.rs at the 0.5.0 tag): --endpoint/-e, --account/-a, and
# --certificate/-c. NOTE: it's --certificate, NOT --certificate-profile
# ~ clap derives the long flag from the field name `certificate`.
# The Certificate *Profile* name is the VALUE we pass to it.
& trusted-signing-cli `
    --endpoint $env:AZURE_ENDPOINT `
    --account $env:AZURE_CODE_SIGNING_ACCOUNT_NAME `
    --certificate $env:AZURE_CERT_PROFILE_NAME `
    $ArtifactPath

if ($LASTEXITCODE -ne 0) {
    Write-Error "[sign-windows] trusted-signing-cli exited $LASTEXITCODE for $ArtifactPath"
    exit $LASTEXITCODE
}

Write-Host "[sign-windows] OK - $ArtifactPath signed."
