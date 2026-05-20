# Invoked by Tauri's `bundle.windows.signCommand` on each Windows
# artifact (NSIS .exe, .msi, WiX extension DLLs). Wraps
# `artifact-signing-cli` so that:
#
#   * CI with Azure secrets   -> sign with Azure Artifact Signing
#   * Local dev (no secrets)  -> skip signing, exit 0, build still wins
#
# This lets the same tauri.conf.json work for everyone without
# branching builds. The bundler calls this script for every Windows
# artifact path it produces; we pass it through to the CLI when we
# have credentials, otherwise we no-op.
#
# tauri.conf.json passes the artifact path as a bare `%1` (NOT
# "%1") ~ Tauri only substitutes the placeholder when an arg is
# exactly `%1`, so quoting it makes the literal string come through.
#
# Required env vars when signing is desired (set in GitHub Actions
# via repo secrets):
#   AZURE_CLIENT_ID                 - Service Principal app ID
#   AZURE_CLIENT_SECRET             - Service Principal secret
#   AZURE_TENANT_ID                 - Entra tenant ID
#   AZURE_ENDPOINT                  - e.g. https://eus.codesigning.azure.net
#   AZURE_CODE_SIGNING_ACCOUNT_NAME - Artifact Signing Account resource name
#   AZURE_CERT_PROFILE_NAME         - Certificate Profile name
#
# Authentication: `artifact-signing-cli` reads
# AZURE_CLIENT_ID / AZURE_CLIENT_SECRET / AZURE_TENANT_ID from the
# environment automatically. We don't pass them on the command line.

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

# artifact-signing-cli wraps signtool.exe + the Microsoft signing
# dlib. Maintained by Levminer; the canonical community CLI for
# this use case in the Tauri ecosystem.
#
# Flag names verified against the pinned v0.11.0: --endpoint/-e,
# --account/-a, and --certificate/-c. NOTE: it's --certificate,
# NOT --certificate-profile ~ clap derives the long flag from the
# field name `certificate`. The Certificate *Profile* name is the
# VALUE we pass to it.
#
# Drop back to "Continue" for the native call. artifact-signing-cli
# shells out to `az login` and `signtool`, both of which write
# progress to stderr. Tauri invokes this script through `powershell`
# (Windows PowerShell 5.1) and captures its output via a pipe ~ and
# under 5.1, a redirected native command's stderr is promoted to
# ErrorRecords, which with ErrorActionPreference=Stop terminates the
# script on the first stderr line. We gate success on $LASTEXITCODE
# explicitly below, so relaxing the preference here costs nothing.
# `2>&1` keeps stderr visible as ordinary build-log output.
$ErrorActionPreference = "Continue"
& artifact-signing-cli `
    --endpoint $env:AZURE_ENDPOINT `
    --account $env:AZURE_CODE_SIGNING_ACCOUNT_NAME `
    --certificate $env:AZURE_CERT_PROFILE_NAME `
    $ArtifactPath 2>&1

if ($LASTEXITCODE -ne 0) {
    Write-Error "[sign-windows] artifact-signing-cli exited $LASTEXITCODE for $ArtifactPath"
    exit $LASTEXITCODE
}

Write-Host "[sign-windows] OK - $ArtifactPath signed."
