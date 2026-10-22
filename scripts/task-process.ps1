# Shared Windows process ownership for development builds and requested checks.
if (-not $IsWindows) { throw 'Task containment requires PowerShell 7 on Windows.' }
if (-not $env:CARGO_BUILD_JOBS) { $env:CARGO_BUILD_JOBS = '2' }
if (-not ('Freym.TaskProcess' -as [type])) {
    Add-Type -Path (Join-Path $PSScriptRoot 'task-process.cs')
}

function Invoke-TaskProcess {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)][string]$FilePath,
        [string[]]$Arguments = @(),
        [string]$WorkingDirectory = (Get-Location).Path,
        [switch]$CaptureOutput,
        [switch]$MergeOutput,
        [string]$LogPath,
        [switch]$Build,
        [Threading.EventWaitHandle]$StopSignal
    )
    $owned = $null
    $log = $null
    $lease = $null
    $hasLease = $false
    $parentOwner = $null
    try {
        if ($env:FREYM_TASK_PARENT_PID) {
            # The npm shim owns this supervisor. Retain its process handle so
            # termination of the shim cannot leave the PowerShell job orphaned.
            $parentOwner = [Diagnostics.Process]::GetProcessById([int]$env:FREYM_TASK_PARENT_PID)
            $parentOwner.Handle | Out-Null
        }
        if ($Build) {
            $root = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..')).ToLowerInvariant()
            $hash = [Convert]::ToHexString([Security.Cryptography.SHA256]::HashData([Text.Encoding]::UTF8.GetBytes($root)))
            $lease = [Threading.Mutex]::new($false, "Local\FreymBuild-$hash")
            try { $hasLease = $lease.WaitOne(0) }
            catch [Threading.AbandonedMutexException] { $hasLease = $true }
            if (-not $hasLease) { throw 'Another build/check owns this checkout. Wait for it to finish; do not start a duplicate.' }
        }
        if ($LogPath) { $log = [IO.StreamWriter]::new($LogPath, $false, [Text.UTF8Encoding]::new($false)) }
        # Serialize arguments as data. Only a fixed, encoded PowerShell launcher is
        # passed to CreateProcess; npm.ps1/.cmd and paths with spaces work too.
        $payload = [Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes((@{
            file = (Get-Command $FilePath -ErrorAction Stop).Source
            arguments = @($Arguments)
        } | ConvertTo-Json -Compress)))
        $command = @"
`$ErrorActionPreference = 'Stop'
`$OutputEncoding = [Console]::OutputEncoding = [Text.UTF8Encoding]::new(`$false)
`$request = [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String('$payload')) | ConvertFrom-Json
try {
    `$commandArguments = @(`$request.arguments)
    & `$request.file @commandArguments
    `$succeeded = `$?
    if (`$null -ne `$LASTEXITCODE) { exit `$LASTEXITCODE }
    if (-not `$succeeded) { exit 1 }
} catch { Write-Error `$_; exit 1 }
"@
        $encoded = [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes($command))
        $pwsh = Join-Path $PSHOME 'pwsh.exe'
        $owned = [Freym.TaskProcess]::new($pwsh, "`"$pwsh`" -NoLogo -NoProfile -NonInteractive -OutputFormat Text -EncodedCommand $encoded", $WorkingDirectory)
        Write-Verbose "Task owner PID $($owned.Id) (private Windows job)."
        do {
            foreach ($line in $owned.DrainOutput()) {
                if ($log) { $log.WriteLine($line.Text); $log.Flush() }
                if ($CaptureOutput -and ($MergeOutput -or -not $line.IsError)) { Write-Output $line.Text } else { Write-Host $line.Text }
            }
            if ($null -ne $StopSignal -and $StopSignal.WaitOne(0)) {
                throw [OperationCanceledException]::new('Task stopped by its owner.')
            }
            if ($null -ne $parentOwner -and $parentOwner.HasExited) {
                throw [OperationCanceledException]::new('Task command owner exited.')
            }
        } while (-not $owned.Wait(100))
        $exitCode = $owned.ExitCode
    } finally {
        try {
            if ($null -ne $owned) {
                $owned.Dispose()
                foreach ($line in $owned.DrainOutput()) {
                    if ($log) { $log.WriteLine($line.Text) }
                    if ($CaptureOutput -and ($MergeOutput -or -not $line.IsError)) { Write-Output $line.Text } else { Write-Host $line.Text }
                }
            }
        } finally {
            if ($log) { $log.Dispose() }
            if ($hasLease) { $lease.ReleaseMutex() }
            if ($lease) { $lease.Dispose() }
            if ($parentOwner) { $parentOwner.Dispose() }
        }
    }
    $global:LASTEXITCODE = $exitCode
}
