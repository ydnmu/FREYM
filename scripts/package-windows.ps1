# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (C) 2026 FREYM contributors
# Compatibility entry point for native GPUI development packaging.
[CmdletBinding()]
param([string]$RepoRoot, [string]$OutputDir, [switch]$SkipInstaller)
$ErrorActionPreference = 'Stop'
& (Join-Path $PSScriptRoot 'package-native-windows.ps1') -RepoRoot $RepoRoot -OutputDir $OutputDir -SkipInstaller:$SkipInstaller
