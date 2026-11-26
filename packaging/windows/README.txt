Radiotrope @VERSION@ for Windows (@ARCH@)
=========================================

An internet radio player: https://github.com/goten002/radiotrope

What is in this folder
----------------------

  radiotrope.exe        The desktop app, and the MCP server for AI agents
  radiotrope-cli.exe    The terminal player
  LICENSE.txt           GNU General Public License, version 3
  THIRD-PARTY-LICENSES.txt
                        Licences of the libraries Radiotrope is built with

Nothing needs installing: unzip the folder anywhere and run radiotrope.exe.
For a Start Menu entry, an uninstaller and updates through
"winget upgrade Radiotrope.Radiotrope", use radiotrope-@VERSION@-setup.exe
instead. This build never checks for updates by itself.

The station checker, radiotrope-probe.exe, is not in this folder: it is a
download of its own on the release page
(radiotrope-probe-@VERSION@-windows-@ARCH@.zip).

Where your settings go
----------------------

By default the same folders as the installed app use, so you can switch
between the two and keep everything:

  %APPDATA%\radiotrope        settings, favorites, stats, schedules
  %LOCALAPPDATA%\radiotrope   cache, log, agent token, paired phones

Portable mode: to keep everything in this folder instead (on a USB stick,
say), create an empty file named "portable" next to radiotrope.exe. In
Explorer: right-click > New > Text Document, and name it "portable" (a file
that ends up as "portable.txt" works too). Radiotrope then keeps all its
files in a "data" folder beside the programs and writes nothing to
%APPDATA% or %LOCALAPPDATA%. The folder must be writable, so not under
"Program Files". A portable copy runs as a player of its own, apart from
an installed one.

Windows SmartScreen
-------------------

These programs are not code-signed yet, so Windows may say "Windows
protected your PC" the first time. Check the download first: the release
page lists a SHA256SUMS file. In PowerShell:

  Get-FileHash .\radiotrope-@VERSION@-windows-@ARCH@.zip

The hash must match the line for that file in SHA256SUMS. Then choose
"More info" > "Run anyway".

AAC+ (HE-AAC) stations
----------------------

AAC+ stations play without anything extra, at AAC-LC quality. For full
HE-AAC, put an fdk-aac.dll (or libfdk-aac-2.dll) built for this
architecture next to radiotrope.exe. Radiotrope does not ship one; see
"AAC+ stations" in the README on GitHub.
