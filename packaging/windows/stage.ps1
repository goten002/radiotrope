<#
.SYNOPSIS
  Stage one architecture's release build and pack its portable zip.

.DESCRIPTION
  Copies the two programs from target\<Target>\release, the licences and
  README.txt into <OutDir>\stage\<Arch>\Radiotrope, checks that each exe is
  built for <Arch> and links the C runtime statically (it must run on a PC
  without the Visual C++ Redistributable), and writes
  <OutDir>\radiotrope-<Version>-windows-<Arch>.zip.

  The portable marker is NOT put in the zip: it runs in normal mode until
  the user adds one (see README.txt).

  With -Probe: the station checker alone, which is in neither the zip nor
  the installer, with the same checks, packaging\probe\README.txt and the
  licences, in <OutDir>\radiotrope-probe-<Version>-windows-<Arch>.zip.

.EXAMPLE
  cargo build --release --locked --target x86_64-pc-windows-msvc -p radiotrope-app -p radiotrope-cli -p radiotrope-probe
  packaging\windows\stage.ps1 -Target x86_64-pc-windows-msvc -Arch x64 -Version 0.1.0
  packaging\windows\stage.ps1 -Target x86_64-pc-windows-msvc -Arch x64 -Version 0.1.0 -Probe
#>
param(
    [Parameter(Mandatory)] [string] $Target,
    [Parameter(Mandatory)] [ValidateSet('x64', 'arm64')] [string] $Arch,
    [Parameter(Mandatory)] [string] $Version,
    [string] $OutDir = 'dist',
    [switch] $Probe
)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$root = Resolve-Path (Join-Path $PSScriptRoot '..\..')
$build = Join-Path $root "target\$Target\release"
$OutDir = [IO.Path]::GetFullPath((Join-Path $root $OutDir))
if ($Probe) {
    $programs = @('radiotrope-probe')
    $name = "radiotrope-probe-$Version-windows-$Arch"
    $stage = Join-Path $OutDir "stage-probe\$Arch\$name"
    $readmeSource = Join-Path $root 'packaging\probe\README.txt'
    $platform = "Windows ($Arch)"
} else {
    $programs = @('radiotrope', 'radiotrope-cli')
    $name = "radiotrope-$Version-windows-$Arch"
    $stage = Join-Path $OutDir "stage\$Arch\Radiotrope"
    $readmeSource = Join-Path $PSScriptRoot 'README.txt'
    $platform = $Arch
}

# IMAGE_FILE_MACHINE_* in the PE header
$machines = @{ x64 = 0x8664; arm64 = 0xAA64 }

function Get-PeMachine([string] $Path) {
    $bytes = [IO.File]::ReadAllBytes($Path)
    $pe = [BitConverter]::ToInt32($bytes, 0x3C)
    return [BitConverter]::ToUInt16($bytes, $pe + 4)
}

# A tool that lists a PE file's imported DLLs: llvm-readobj (both runner
# images have LLVM), or dumpbin from Visual Studio
function Get-Imports([string] $Path) {
    $readobj = (Get-Command llvm-readobj -ErrorAction SilentlyContinue).Source
    if (-not $readobj -and (Test-Path "$env:ProgramFiles\LLVM\bin\llvm-readobj.exe")) {
        $readobj = "$env:ProgramFiles\LLVM\bin\llvm-readobj.exe"
    }
    if ($readobj) {
        return (& $readobj --coff-imports $Path | Select-String '^\s*Name:\s*(\S+\.dll)' |
                ForEach-Object { $_.Matches[0].Groups[1].Value })
    }
    $vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
    $dumpbin = & $vswhere -latest -products * -find 'VC\Tools\MSVC\**\bin\Host*\*\dumpbin.exe' | Select-Object -First 1
    if (-not $dumpbin) { throw 'Neither llvm-readobj nor dumpbin was found' }
    return (& $dumpbin /dependents $Path | Select-String '^\s+(\S+\.dll)\s*$' |
            ForEach-Object { $_.Matches[0].Groups[1].Value })
}

if (Test-Path $stage) { Remove-Item -Recurse -Force $stage }
New-Item -ItemType Directory -Force $stage | Out-Null

foreach ($p in $programs) {
    $exe = Join-Path $build "$p.exe"
    if (-not (Test-Path $exe)) { throw "$exe is missing: build the release first" }

    $machine = Get-PeMachine $exe
    if ($machine -ne $machines[$Arch]) {
        throw ("{0} is built for machine 0x{1:X4}, not {2}" -f $exe, $machine, $Arch)
    }
    $imports = @(Get-Imports $exe)
    Write-Host "$p.exe imports: $($imports -join ', ')"
    $crt = $imports | Where-Object { $_ -match '^(vcruntime|msvcp|ucrtbase|api-ms-win-crt-)' }
    if ($crt) {
        throw "$p.exe needs the C runtime DLLs ($($crt -join ', ')): build with RUSTFLAGS='-C target-feature=+crt-static'"
    }
    $fdk = $imports | Where-Object { $_ -match 'fdk' }
    if ($fdk) { throw "$p.exe links FDK AAC ($fdk); it must only load it at run time" }
    Copy-Item $exe $stage
}

Copy-Item (Join-Path $root 'LICENSE') (Join-Path $stage 'LICENSE.txt')
Copy-Item (Join-Path $root 'THIRD-PARTY-LICENSES') (Join-Path $stage 'THIRD-PARTY-LICENSES.txt')
$readme = Get-Content -Raw $readmeSource
$readme = $readme.Replace('@VERSION@', $Version).Replace('@ARCH@', $Arch).Replace('@PLATFORM@', $platform)
# Notepad copes with LF, but CRLF is what Windows users expect
$readme = $readme -replace "`r?`n", "`r`n"
[IO.File]::WriteAllText((Join-Path $stage 'README.txt'), $readme, [Text.UTF8Encoding]::new($false))

if (Get-ChildItem $stage -Filter 'portable*') { throw 'The zip must not contain the portable marker' }

$zip = Join-Path $OutDir "$name.zip"
if (Test-Path $zip) { Remove-Item -Force $zip }
Compress-Archive -Path $stage -DestinationPath $zip -CompressionLevel Optimal
Get-ChildItem $stage | Format-Table Name, Length -AutoSize | Out-String | Write-Host
Write-Host "Wrote $zip"
