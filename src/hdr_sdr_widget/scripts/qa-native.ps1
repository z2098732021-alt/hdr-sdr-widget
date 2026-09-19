#requires -Version 7.0
param(
    [string]$SettingsTemplate=(Join-Path $env:APPDATA 'hdr-sdr-widget/settings.json'),
    [string]$Binary=(Join-Path $PSScriptRoot '../src-tauri/target/release/hdr-sdr-widget.exe'),
    [string]$OutputDirectory=(Join-Path $PSScriptRoot '../../../deliverables/v0.4.0-native/qa/repro'),
    [ValidateSet('Benchmark','Dda','Clarity','Docked','DockedLeft','Material','Record','Legacy','Optics','OpticsBaseline','Brightness')][string]$Mode='Benchmark',
    [ValidateRange(10,3600)][int]$Seconds=30
)
$ErrorActionPreference='Stop'
$executable=(Resolve-Path -LiteralPath $Binary).Path
New-Item -ItemType Directory -Path $OutputDirectory -Force | Out-Null
$output=(Resolve-Path -LiteralPath $OutputDirectory).Path
$profile=Join-Path $output 'profile'
New-Item -ItemType Directory -Path $profile -Force | Out-Null
$settingsPath=$SettingsTemplate
if(Test-Path -LiteralPath $settingsPath){
    $settings=Get-Content -LiteralPath $settingsPath -Raw -Encoding utf8 | ConvertFrom-Json
    $settings.hotkey='';$settings.firstRun=$false
    if($Mode -in @('Docked','Record')){$settings.dockSide='right'}elseif($Mode -eq 'DockedLeft'){$settings.dockSide='left'}else{$settings.dockSide=$null}
    $settings | ConvertTo-Json -Depth 12 | Set-Content -LiteralPath (Join-Path $profile 'settings.json') -Encoding utf8
}
$variables=@('HSDR_CONFIG_DIR','HSDR_STATUS_DUMP','HSDR_STATUS_DELAY','HSDR_STATUS_KEEP','HSDR_BENCHMARK','HSDR_NATIVE_TEST','HSDR_FORCE_DDA','HSDR_VISUAL_AUDIT','HSDR_RENDERER','HSDR_EDGE_TEST','HSDR_EDGE_LEFT','HSDR_MATERIAL_TEST','HSDR_RECORD','HSDR_OPTICS_TEST','HSDR_OPTICS_BASELINE','HSDR_BRIGHTNESS_AUDIT')
$saved=@{}
foreach($name in $variables){$saved[$name]=[Environment]::GetEnvironmentVariable($name,'Process');Remove-Item -LiteralPath "Env:$name" -ErrorAction SilentlyContinue}
try {
    $env:HSDR_CONFIG_DIR=$profile
    $env:HSDR_STATUS_DUMP=Join-Path $output 'diagnostics.json'
    $env:HSDR_STATUS_DELAY=($Seconds*1000).ToString()
    if($Mode -eq 'Benchmark'){$env:HSDR_BENCHMARK='1'}
    if($Mode -eq 'Dda'){$env:HSDR_FORCE_DDA='1';$env:HSDR_BENCHMARK='1'}
    if($Mode -eq 'Docked'){$env:HSDR_EDGE_TEST='1'}
    if($Mode -eq 'DockedLeft'){$env:HSDR_EDGE_TEST='1';$env:HSDR_EDGE_LEFT='1'}
    if($Mode -eq 'Material'){$env:HSDR_NATIVE_TEST=$output;$env:HSDR_MATERIAL_TEST='1'}
    if($Mode -eq 'Record'){$env:HSDR_EDGE_TEST='1';$env:HSDR_RECORD=$output}
    if($Mode -eq 'Clarity'){$env:HSDR_NATIVE_TEST=$output}
    if($Mode -in @('Optics','OpticsBaseline')){$env:HSDR_NATIVE_TEST=$output;$env:HSDR_OPTICS_TEST='1'}
    if($Mode -eq 'OpticsBaseline'){$env:HSDR_OPTICS_BASELINE='1'}
    if($Mode -eq 'Brightness'){$env:HSDR_BRIGHTNESS_AUDIT=Join-Path $output 'brightness-audit.json'}
    if($Mode -eq 'Legacy'){$env:HSDR_RENDERER='legacy'}
    $startedUtc=[DateTime]::UtcNow
    [pscustomobject]@{binary=$executable;sha256=(Get-FileHash -LiteralPath $executable -Algorithm SHA256).Hash;mode=$Mode;durationSeconds=$Seconds;startedUtc=$startedUtc.ToString('o')} | ConvertTo-Json | Set-Content -LiteralPath (Join-Path $output 'run.json') -Encoding utf8
    $process=Start-Process -FilePath $executable -WindowStyle Hidden -PassThru -RedirectStandardError (Join-Path $output 'stderr.log')
    $started=[Diagnostics.Stopwatch]::StartNew()
    $samples=[Collections.Generic.List[object]]::new()
    while(!$process.WaitForExit(1000)){
        $process.Refresh()
        $samples.Add([pscustomobject]@{seconds=$started.Elapsed.TotalSeconds;privateBytes=$process.PrivateMemorySize64;workingSetBytes=$process.WorkingSet64;handles=$process.HandleCount;cpuSeconds=$process.TotalProcessorTime.TotalSeconds})
        if($started.Elapsed.TotalSeconds -gt $Seconds+30){throw 'Application did not exit after the diagnostic deadline'}
    }
    $samples | Export-Csv -LiteralPath (Join-Path $output 'resources.csv') -NoTypeInformation -Encoding utf8
    if($process.ExitCode -ne 0){throw "Application exited with $($process.ExitCode)"}
    if(!(Test-Path -LiteralPath $env:HSDR_STATUS_DUMP)){throw 'No diagnostic report: close an already running widget before starting this test'}
    if((Get-Item -LiteralPath $env:HSDR_STATUS_DUMP).LastWriteTimeUtc -lt $startedUtc){throw 'Diagnostic report belongs to a previous run'}
    if($Mode -ne 'Legacy') {
        $report=Get-Content -LiteralPath $env:HSDR_STATUS_DUMP -Raw -Encoding utf8 | ConvertFrom-Json
        if($report.error){throw "Native rendering failed: $($report.error)"}
        if($report.submitted -eq 0){throw 'No native frames were submitted'}
    }
    if($Mode -eq 'Brightness') {
        $audit=Get-Content -LiteralPath (Join-Path $output 'brightness-audit.json') -Raw -Encoding utf8 | ConvertFrom-Json
        if($audit.error -or @($audit.cases | Where-Object { !$_.passed }).Count -gt 0){throw 'Brightness integration audit failed'}
    }
    Get-Content -LiteralPath $env:HSDR_STATUS_DUMP -Raw -Encoding utf8
} finally {
    foreach($name in $variables){if($null -eq $saved[$name]){Remove-Item -LiteralPath "Env:$name" -ErrorAction SilentlyContinue}else{[Environment]::SetEnvironmentVariable($name,$saved[$name],'Process')}}
}
