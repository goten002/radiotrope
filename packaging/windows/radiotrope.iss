; Radiotrope: one installer for Windows x64 and ARM64 (Inno Setup 6.3 or newer)
;
;   iscc /DAppVersion=0.1.0 /DSourceX64=<dir> /DSourceArm64=<dir> /DOutputDir=<dir> radiotrope.iss
;
; SourceX64 and SourceArm64 are the staged folders that stage.ps1 makes
; (dist\stage\<arch>\Radiotrope). The x86 setup program runs on both; it
; installs the ARM64 programs on ARM64 Windows and the x64 ones elsewhere.
;
; Per-user by default (no administrator prompt) into
; %LOCALAPPDATA%\Programs\Radiotrope; "Install for all users" goes to
; Program Files. Silent: /VERYSILENT /SUPPRESSMSGBOXES [/ALLUSERS | /CURRENTUSER].
; /RELAUNCH starts the player again after a silent install: the player's own
; update (Help > Check for Updates) runs the new setup program with
; /SILENT /SUPPRESSMSGBOXES /NORESTART /RELAUNCH (src/update/install.rs).
; The uninstaller takes /DELETEDATA to also remove settings, favorites and
; cache without asking.

#ifndef AppVersion
  #error Pass the version: iscc /DAppVersion=0.1.0 radiotrope.iss
#endif
#ifndef SourceX64
  #define SourceX64 "..\..\dist\stage\x64\Radiotrope"
#endif
#ifndef SourceArm64
  #define SourceArm64 "..\..\dist\stage\arm64\Radiotrope"
#endif
#ifndef OutputDir
  #define OutputDir "..\..\dist"
#endif

#define AppName "Radiotrope"
#define AppExe "radiotrope.exe"

[Setup]
; PERMANENT: Windows finds an earlier install (for upgrades and uninstall)
; by this id. Never change it. See packaging\windows\README.md.
AppId={{F8B5B3D8-4BAC-4AE4-9925-5AA527345B2E}
AppName={#AppName}
AppVersion={#AppVersion}
AppVerName={#AppName} {#AppVersion}
AppPublisher=Radiotrope
AppPublisherURL=https://radiotrope.app
AppSupportURL=https://github.com/goten002/radiotrope/issues
AppUpdatesURL=https://github.com/goten002/radiotrope/releases
AppCopyright=Copyright 2026 George Alexiou
; The name and version in Installed apps, which WinGet matches
UninstallDisplayName={#AppName}
UninstallDisplayIcon={app}\{#AppExe}
VersionInfoVersion={#AppVersion}
VersionInfoProductName={#AppName}
VersionInfoDescription={#AppName} Setup

DefaultDirName={autopf}\{#AppName}
DefaultGroupName={#AppName}
DisableProgramGroupPage=yes
DisableDirPage=auto
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog commandline
ArchitecturesAllowed=x64compatible or arm64
ArchitecturesInstallIn64BitMode=x64compatible or arm64
MinVersion=10.0

; A player left in the tray is closed with `radiotrope --quit` before files
; are replaced (see [Code]); the Restart Manager is the fallback
CloseApplications=yes
RestartApplications=no

OutputDir={#OutputDir}
OutputBaseFilename=radiotrope-{#AppVersion}-setup
SetupIconFile=..\..\assets\icons\radiotrope.ico
WizardStyle=modern
Compression=lzma2/max
SolidCompression=yes
; Not signed yet: see packaging\windows\README.md

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
Source: "{#SourceX64}\radiotrope.exe"; DestDir: "{app}"; Check: not IsArm64; Flags: ignoreversion
Source: "{#SourceX64}\radiotrope-cli.exe"; DestDir: "{app}"; Check: not IsArm64; Flags: ignoreversion
Source: "{#SourceX64}\radiotrope-probe.exe"; DestDir: "{app}"; Check: not IsArm64; Flags: ignoreversion
Source: "{#SourceArm64}\radiotrope.exe"; DestDir: "{app}"; Check: IsArm64; Flags: ignoreversion
Source: "{#SourceArm64}\radiotrope-cli.exe"; DestDir: "{app}"; Check: IsArm64; Flags: ignoreversion
Source: "{#SourceArm64}\radiotrope-probe.exe"; DestDir: "{app}"; Check: IsArm64; Flags: ignoreversion
Source: "{#SourceX64}\README.txt"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#SourceX64}\LICENSE.txt"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#SourceX64}\THIRD-PARTY-LICENSES.txt"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\{#AppName}"; Filename: "{app}\{#AppExe}"
Name: "{autodesktop}\{#AppName}"; Filename: "{app}\{#AppExe}"; Tasks: desktopicon

[Registry]
; View > System Tray > Start at Login writes this value (src/autostart.rs);
; the installer never creates it, but the uninstaller removes it
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueType: none; ValueName: "Radiotrope"; Flags: uninsdeletevalue dontcreatekey

[Run]
Filename: "{app}\{#AppExe}"; Description: "{cm:LaunchProgram,{#AppName}}"; Flags: nowait postinstall skipifsilent
; The player's own update: back to the player, as the user who started it
; (not elevated after an install for all users)
Filename: "{app}\{#AppExe}"; Flags: nowait runasoriginaluser; Check: WantsRelaunch

[Code]
// Close a running player (it may be hidden in the tray) and wait for it
// to exit, so its files can be replaced or removed. Finding none is fine;
// a failure leaves it to the Restart Manager.
procedure QuitRunningPlayer();
var
  Exe: String;
  Code: Integer;
begin
  Exe := ExpandConstant('{app}\{#AppExe}');
  if not FileExists(Exe) then
    Exit;
  if Exec(Exe, '--quit', '', SW_HIDE, ewWaitUntilTerminated, Code) then
    Log(Format('%s --quit exited with %d', [Exe, Code]))
  else
    Log(Format('%s --quit could not run: %s', [Exe, SysErrorMessage(Code)]));
end;

function PrepareToInstall(var NeedsRestart: Boolean): String;
begin
  QuitRunningPlayer();
  Result := '';
end;

function InitializeUninstall(): Boolean;
begin
  QuitRunningPlayer();
  Result := True;
end;

function HasParam(const Name: String): Boolean;
var
  I: Integer;
begin
  Result := False;
  for I := 1 to ParamCount do
    if CompareText(ParamStr(I), Name) = 0 then
      Result := True;
end;

// /RELAUNCH: start the player once the files are in place
function WantsRelaunch(): Boolean;
begin
  Result := HasParam('/RELAUNCH');
end;

// After the programs are gone: offer to delete the user's own data too.
// Silent uninstalls (WinGet's) keep it unless /DELETEDATA is given.
procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
var
  Delete: Boolean;
begin
  if CurUninstallStep <> usPostUninstall then
    Exit;
  if HasParam('/DELETEDATA') then
    Delete := True
  else if UninstallSilent then
    Delete := False
  else
    Delete := MsgBox('Also delete your Radiotrope settings, favorites, stats and cache?' + #13#10#13#10 +
      'They are in:' + #13#10 +
      ExpandConstant('{userappdata}\radiotrope') + #13#10 +
      ExpandConstant('{localappdata}\radiotrope') + #13#10#13#10 +
      'Choose No to keep them for a later install. Agents set up to use Radiotrope ' +
      'still name it in their MCP settings; remove it there too.',
      mbConfirmation, MB_YESNO or MB_DEFBUTTON2) = IDYES;
  if Delete then
  begin
    DelTree(ExpandConstant('{userappdata}\radiotrope'), True, True, True);
    DelTree(ExpandConstant('{localappdata}\radiotrope'), True, True, True);
  end;
end;
