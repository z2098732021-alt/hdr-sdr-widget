#requires -Version 7.0
param([switch]$Debug,[switch]$SkipFrontend)
$ErrorActionPreference='Stop'
$projectRoot=Split-Path -Parent $PSScriptRoot
Push-Location $projectRoot
try {
    $env:CARGO_TARGET_DIR=Join-Path $projectRoot 'src-tauri/target'
    if (!(Test-Path -LiteralPath 'node_modules/@tauri-apps/cli/tauri.js')) {
        npm ci
        if ($LASTEXITCODE) {throw 'npm ci failed'}
    }
    $arguments=@('node_modules/@tauri-apps/cli/tauri.js','build','--bundles','nsis')
    if($Debug){$arguments+='--debug'}
    if($SkipFrontend){$arguments+=@('--config','{"build":{"beforeBuildCommand":""}}')}
    node @arguments
    if($LASTEXITCODE){throw 'Tauri build failed'}
} finally {Pop-Location}
