# Invoked by Tauri's `bundle.windows.signCommand` on each Windows
# artifact (NSIS .exe, .msi). Wraps `artifact-signing-cli` so that:
#
#   * CI with Azure secrets   -> sign with Azure Artifact Signing
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
#   AZURE_CODE_SIGNING_ACCOUNT_NAME - Artifact Signing Account resource name
#   AZURE_CERT_PROFILE_NAME         - Certificate Profile name
#
# Authentication: `artifact-signing-cli` reads
# AZURE_CLIENT_ID / AZURE_CLIENT_SECRET / AZURE_TENANT_ID from the
# environment automatically. We don't pass them on the command line.
#
# DEBUG NOTE: Tauri's bundler captures this script's stdout/stderr and
# logs it only at `debug` level ~ then discards it entirely if the
# script exits non-zero (see tauri-bundler CommandExt::output_ok). So
# a failing sign step only ever surfaces a useless "failed to run
# powershell". To get around that, every milestone below is also
# appended to a log file ($RUNNER_TEMP\packrelay-sign-windows.log),
# which release.yml dumps in an `if: always()` step.

param(
    [Parameter(Mandatory = $true, Position = 0)]
    [string]$ArtifactPath
)

$ErrorActionPreference = "Stop"

# Persistent log file ~ the only reliable channel out of this script,
# since Tauri swallows the console streams. Append: the bundler calls
# this script once per artifact (raw .exe, NSIS .exe, .msi).
$logDir = if ($env:RUNNER_TEMP) { $env:RUNNER_TEMP } else { $env:TEMP }
$logPath = Join-Path $logDir "packrelay-sign-windows.log"

function Log($msg) {
    $line = "$(Get-Date -Format o) [sign-windows] $msg"
    Write-Host $line
    try { Add-Content -LiteralPath $logPath -Value $line -ErrorAction Stop } catch { }
}

Log "invoked  artifact=$ArtifactPath"
Log "host     PSVersion=$($PSVersionTable.PSVersion) PID=$PID CWD=$(Get-Location)"

# If creds aren't present, signing is intentionally skipped. This
# is the path local devs and PR-builds take. The artifact stays
# unsigned but the build itself succeeds.
if (-not $env:AZURE_CLIENT_SECRET) {
    Log "AZURE_CLIENT_SECRET not set - skipping signing"
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
        Log "ERROR required env var '$var' is not set"
        exit 1
    }
}

Log "signing  $ArtifactPath"

# artifact-signing-cli wraps signtool.exe + the Microsoft signing
# dlib. Maintained by Levminer; the canonical community CLI for
# this exact use case in the Tauri ecosystem.
#
# Flag names verified against the pinned v0.11.0: --endpoint/-e,
# --account/-a, and --certificate/-c. NOTE: it's --certificate,
# NOT --certificate-profile ~ clap derives the long flag from the
# field name `certificate`. The Certificate *Profile* name is the
# VALUE we pass to it.
#
# Drop back to "Continue" for the native call. artifact-signing-cli
# shells out to `az login` and `signtool`, both of which write
# progress/info to stderr. Tauri invokes this script through
# `powershell` (Windows PowerShell 5.1) and captures its output via
# a pipe ~ and under 5.1, a redirected native command's stderr gets
# promoted to ErrorRecords, which with ErrorActionPreference=Stop
# *terminates the script on the first stderr line*. We gate success
# on $LASTEXITCODE explicitly below, so relaxing the preference here
# costs us nothing. Capture the full output (2>&1) so it lands in the
# log file regardless of what Tauri does with the console streams.
$ErrorActionPreference = "Continue"
$cliOutput = & artifact-signing-cli `
    --endpoint $env:AZURE_ENDPOINT `
    --account $env:AZURE_CODE_SIGNING_ACCOUNT_NAME `
    --certificate $env:AZURE_CERT_PROFILE_NAME `
    $ArtifactPath 2>&1 | Out-String
$cliExit = $LASTEXITCODE

Log "artifact-signing-cli exit=$cliExit  output follows >>>"
Write-Host $cliOutput
try { Add-Content -LiteralPath $logPath -Value $cliOutput -ErrorAction Stop } catch { }
Log "<<< end artifact-signing-cli output"

if ($cliExit -ne 0) {
    Log "ERROR artifact-signing-cli exited $cliExit for $ArtifactPath"
    exit $cliExit
}

Log "OK - $ArtifactPath signed"
exit 0
