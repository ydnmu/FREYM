# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (C) 2026 FREYM contributors
<# Native GPUI development packaging. Builds once, stages the existing launcher,
   and creates portable/NSIS artifacts only after the source and notice gate.
   Never launches the application or installer, signs, uploads or publishes. #>
[CmdletBinding()]
param([string]$RepoRoot, [string]$OutputDir, [switch]$SkipInstaller, [string]$MakensisPath, [string]$VcRuntimePath)
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
if (-not $IsWindows) { throw 'Native Windows packaging requires PowerShell 7 on Windows.' }
. (Join-Path $PSScriptRoot 'task-process.ps1')
if (-not $RepoRoot) { $RepoRoot = Split-Path -Parent $PSScriptRoot }
$RepoRoot = (Resolve-Path -LiteralPath $RepoRoot).Path
if (-not $OutputDir) { $OutputDir = Join-Path $RepoRoot 'test-results/windows-devbuild' }
$OutputDir = [IO.Path]::GetFullPath($OutputDir)
if (Test-Path -LiteralPath $OutputDir) { throw 'Choose a new output directory; previous artifacts are preserved.' }
if (-not $SkipInstaller) {
    if (-not $MakensisPath) {
        $tool = Get-Command makensis -ErrorAction SilentlyContinue
        if ($tool) { $MakensisPath = $tool.Source }
        else { $MakensisPath = Join-Path $env:ProgramFiles 'NSIS/makensis.exe' }
    }
    if (-not (Test-Path -LiteralPath $MakensisPath -PathType Leaf)) {
        throw 'NSIS 3.11 is required for the installer. Supply -MakensisPath or use -SkipInstaller for a portable artifact.'
    }
}
New-Item -ItemType Directory -Path $OutputDir | Out-Null
Push-Location $RepoRoot
try {
    $snapshotTool = @('-B', 'scripts/prepare-native-resources.py', '--repo-root', $RepoRoot,
        '--output', (Join-Path $OutputDir 'resources'))
    Invoke-TaskProcess -FilePath python -Arguments $snapshotTool -Build -LogPath (Join-Path $OutputDir 'resources.log')
    if ($LASTEXITCODE -ne 0) { throw 'Native notice/source gate failed. Evidence is preserved in the output directory; no distribution was created.' }
    $manifestPath = Join-Path $OutputDir 'resources/SOURCE-MANIFEST.json'
    $sourceManifest = Get-Content -LiteralPath $manifestPath -Raw | ConvertFrom-Json
    $metadata = & cargo metadata --locked --offline --no-deps --format-version 1 | ConvertFrom-Json
    if ($LASTEXITCODE -ne 0) { throw 'Could not locate the configured Cargo target directory.' }
    $version = @($metadata.packages | Where-Object name -eq 'freym-app')[0].version
    $release = Join-Path $metadata.target_directory 'release'
    # Preserve existing executable artifacts before the single owned Cargo build.
    $backup = Join-Path $OutputDir 'previous-binaries'
    New-Item -ItemType Directory -Path $backup | Out-Null
    foreach ($name in @('freym.exe', 'freym.pdb', 'freym-launcher.exe', 'freym_launcher.pdb')) {
        $file = Join-Path $release $name
        if (Test-Path -LiteralPath $file -PathType Leaf) { Copy-Item -LiteralPath $file -Destination (Join-Path $backup $name) }
    }
    Invoke-TaskProcess -FilePath cargo -Arguments @('build', '--locked', '--offline', '--release',
        '-p', 'freym-app', '--bin', 'freym', '-p', 'freym-update', '--bin', 'freym-launcher', '-j2') -Build -LogPath (Join-Path $OutputDir 'build.log')
    if ($LASTEXITCODE -ne 0) { throw 'Native application/launcher build failed; no distribution was created.' }
    Invoke-TaskProcess -FilePath python -Arguments @('-B', 'scripts/prepare-native-resources.py',
        '--repo-root', $RepoRoot, '--verify', $manifestPath)
    if ($LASTEXITCODE -ne 0) { throw 'Source changed during the candidate build.' }
    $portable = Join-Path $OutputDir 'portable'
    if (-not $VcRuntimePath) {
        $vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio/Installer/vswhere.exe'
        if (-not (Test-Path -LiteralPath $vswhere -PathType Leaf)) { throw 'Supply -VcRuntimePath from the licensed Visual Studio x64 VC/Redist tree.' }
        $vs = & $vswhere -latest -products '*' -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
        if ($LASTEXITCODE -ne 0 -or -not $vs) { throw 'Could not locate the native compiler redistributables.' }
        $candidates = @(Get-ChildItem -Path (Join-Path $vs 'VC/Redist/MSVC/*/x64/Microsoft.VC*.CRT/vcruntime140.dll') -File | Sort-Object { [version]$_.VersionInfo.FileVersion } -Descending)
        if (-not $candidates.Count) { throw 'The required x64 release VCRUNTIME140.dll is absent from VC/Redist.' }
        $VcRuntimePath = $candidates[0].FullName
    }
    Invoke-TaskProcess -FilePath python -Arguments @('-B', 'scripts/prepare-native-portable.py',
        '--app', (Join-Path $release 'freym.exe'), '--launcher', (Join-Path $release 'freym-launcher.exe'),
        '--release-resources', (Join-Path $OutputDir 'resources'), '--output', $portable,
        '--vc-runtime', $VcRuntimePath, '--vc-runtime-license', (Join-Path $RepoRoot 'scripts/license-provenance/microsoft-vc-v14-runtime.docx')) -LogPath (Join-Path $OutputDir 'staging.log')
    if ($LASTEXITCODE -ne 0) { throw 'Native portable staging failed.' }
    $packageManifest = Get-Content -LiteralPath (Join-Path $portable 'PORTABLE-MANIFEST.json') -Raw | ConvertFrom-Json
    if (-not $SkipInstaller) {
        if ($packageManifest.updatePackage -ne 'disabled') {
            throw 'This current-user installer cannot claim portable updater ownership. Use disabled installer update trust or a separately qualified installer update implementation.'
        }
        Invoke-TaskProcess -FilePath $MakensisPath -Arguments @('/VERSION') -CaptureOutput | Set-Content -LiteralPath (Join-Path $OutputDir 'nsis-version.txt')
        if ($LASTEXITCODE -ne 0 -or (Get-Content -LiteralPath (Join-Path $OutputDir 'nsis-version.txt') -Raw).Trim() -ne 'v3.11') {
            throw 'The reviewed native installer requires NSIS 3.11.'
        }
        $deletes = [Collections.Generic.List[string]]::new()
        $directories = [Collections.Generic.HashSet[string]]::new()
        foreach ($file in Get-ChildItem -LiteralPath $portable -File -Recurse) {
            $relative = [IO.Path]::GetRelativePath($portable, $file.FullName)
            if ($relative.IndexOfAny([char[]]'$"') -ge 0) { throw 'Unsafe NSIS payload filename.' }
            $deletes.Add('Delete /REBOOTOK "$INSTDIR\' + $relative + '"')
            $parent = Split-Path -Parent $relative
            while ($parent) { [void]$directories.Add($parent); $parent = Split-Path -Parent $parent }
        }
        foreach ($directory in $directories | Sort-Object Length -Descending) { $deletes.Add('RMDir "$INSTDIR\' + $directory + '"') }
        $uninstallList = Join-Path $OutputDir 'uninstall-files.nsh'
        [IO.File]::WriteAllLines($uninstallList, $deletes, [Text.UTF8Encoding]::new($false))
        $installer = Join-Path $OutputDir "FREYM-$version-windows-x64-setup.exe"
        Invoke-TaskProcess -FilePath $MakensisPath -Arguments @('/V2', "/DPAYLOAD=$portable", "/DOUTPUT=$installer",
            "/DVERSION=$version", "/DUNINSTALL_LIST=$uninstallList", "/DICON=$RepoRoot/assets/brand/icon.ico",
            (Join-Path $RepoRoot 'scripts/native-installer.nsi')) -Build -LogPath (Join-Path $OutputDir 'installer.log')
        if ($LASTEXITCODE -ne 0) { throw 'Native installer compilation failed.' }
    }
    $zip = Join-Path $OutputDir "FREYM-$version-windows-x64-portable.zip"
    Compress-Archive -LiteralPath @(Get-ChildItem -LiteralPath $portable | Select-Object -ExpandProperty FullName) -DestinationPath $zip
    $artifacts = @(Get-ChildItem -LiteralPath $OutputDir -File | Where-Object Extension -in @('.exe','.zip') | ForEach-Object {
        [ordered]@{ name=$_.Name; bytes=$_.Length; sha256=(Get-FileHash -LiteralPath $_.FullName -Algorithm SHA256).Hash.ToLowerInvariant() }
    })
    [ordered]@{ schemaVersion=1; product='FREYM native GPUI'; version=$version; signed=$false;
        sourceManifestSha256=(Get-FileHash -LiteralPath $manifestPath -Algorithm SHA256).Hash.ToLowerInvariant();
        source=$sourceManifest; artifacts=$artifacts; appExecuted=$false; installerExecuted=$false;
        productionUpdates=($packageManifest.updatePackage -ne 'disabled'); runtimeNote='Native recorder needs no Node, WebView2 or FFmpeg.'
    } | ConvertTo-Json -Depth 8 | Set-Content -LiteralPath (Join-Path $OutputDir 'MANIFEST.json') -Encoding utf8
    Write-Host "Native development artifacts: $OutputDir (unsigned; not published or installed)."
} finally { Pop-Location }
