[CmdletBinding()]
param(
    [string]$DataRoot = (Join-Path (Split-Path -Parent $PSScriptRoot) 'vibex-development-data'),
    [switch]$Tui
)
$ErrorActionPreference = 'Stop'
$DataRoot = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($DataRoot)
New-Item -ItemType Directory -Path $DataRoot -Force | Out-Null
# Desktop appends desktop-preview to VIBEX_DB_PATH's parent; the TUI consumes
# VIBEX_HOME directly. Both must reach the same isolated development runtime.
$env:VIBEX_DB_PATH = Join-Path $DataRoot 'vibex.db'
$env:VIBEX_HOME = Join-Path $DataRoot 'desktop-preview'
$env:VIBEX_CHANNEL = 'preview'
$env:VIBEX_INSTALL_PACKAGE = 'unmanaged'
$env:TEMP = Join-Path $DataRoot 'temp'
$env:TMP = $env:TEMP
New-Item -ItemType Directory -Path $env:VIBEX_HOME,$env:TEMP -Force | Out-Null
if ($Tui) {
    & (Join-Path $PSScriptRoot 'vibex.exe')
    exit $LASTEXITCODE
}
Start-Process -FilePath (Join-Path $PSScriptRoot 'vibex-desktop.exe') -WorkingDirectory $PSScriptRoot -WindowStyle Normal
