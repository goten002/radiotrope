<#
.SYNOPSIS
  Fill in the WinGet manifest templates for one release.

.DESCRIPTION
  Writes manifests/r/Radiotrope/Radiotrope/<Version>/ under -OutDir, ready to
  check with `winget validate` and `winget install --manifest`, and to copy
  into a fork of microsoft/winget-pkgs for the first, manual submission.
  The SHA-256 is taken from -Installer (a local copy of the release's
  radiotrope-<Version>-setup.exe), or downloaded from the release.

.EXAMPLE
  packaging\windows\winget\render.ps1 -Version 0.1.0 -Installer dist\radiotrope-0.1.0-setup.exe
#>
param(
    [Parameter(Mandatory)] [string] $Version,
    [string] $Installer,
    [string] $OutDir = 'dist\winget',
    [string] $ReleaseDate = (Get-Date -Format 'yyyy-MM-dd')
)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

if (-not $Installer) {
    $url = "https://github.com/goten002/radiotrope/releases/download/v$Version/radiotrope-$Version-setup.exe"
    $Installer = Join-Path ([IO.Path]::GetTempPath()) "radiotrope-$Version-setup.exe"
    Invoke-WebRequest -Uri $url -OutFile $Installer
}
$sha = (Get-FileHash -Algorithm SHA256 $Installer).Hash

$dest = Join-Path $OutDir "manifests\r\Radiotrope\Radiotrope\$Version"
New-Item -ItemType Directory -Force $dest | Out-Null
foreach ($template in Get-ChildItem $PSScriptRoot -Filter 'Radiotrope.Radiotrope*.yaml') {
    $text = Get-Content -Raw $template.FullName
    # The template's own header comment is not part of the manifest
    $text = ($text -split "`n" | Where-Object { $_ -notmatch '^# (Template for|Filled in by)' }) -join "`n"
    $text = $text.Replace('@VERSION@', $Version).Replace('@SHA256@', $sha).Replace('@DATE@', $ReleaseDate)
    [IO.File]::WriteAllText((Join-Path $dest $template.Name), $text, [Text.UTF8Encoding]::new($false))
}
Write-Host "Wrote $dest (installer SHA-256 $sha)"
