# Run one requested command in the foreground; clean up its entire owned tree.
$ErrorActionPreference = 'Stop'
if ($args.Count -eq 0) { throw 'Supply a command followed by its arguments.' }
$command = $args[0]
$commandArguments = @($args | Select-Object -Skip 1)
. (Join-Path $PSScriptRoot 'task-process.ps1')
Invoke-TaskProcess -FilePath $command -Arguments $commandArguments -Build
exit $LASTEXITCODE
