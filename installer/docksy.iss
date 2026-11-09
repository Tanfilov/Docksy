; Docksy installer (Inno Setup 6). Build: build-installer.ps1, or
;   ISCC.exe installer\docksy.iss   (after cargo build --release)
;
; Installs per-machine into Program Files (admin once). The tray hook DLL must
; live next to the exe (Explorer loads it from there), and Docksy writes a fresh
; copy of it into {app}\hook on every start, so that folder is user-writable.

#define AppName "Docksy"
#define AppVersion "0.1.0"
#define AppExe "docksy.exe"
#define BuildDir "..\target\aarch64-pc-windows-msvc\release"

[Setup]
AppId={{6B2F3C1E-8D4A-4F7B-9E21-D0C5A7B3E914}
AppName={#AppName}
AppVersion={#AppVersion}
AppVerName={#AppName} {#AppVersion}
AppPublisher={#AppName}
AppComments=Lightweight big-icon taskbar for Windows 11. Based on Seelen UI by eythaann (AGPL-3.0).
DefaultDirName={autopf}\{#AppName}
DisableProgramGroupPage=yes
DisableDirPage=auto
PrivilegesRequired=admin
ArchitecturesAllowed=arm64
ArchitecturesInstallIn64BitMode=arm64
LicenseFile=..\LICENSE
SetupIconFile=..\taskbar\assets\docksy.ico
; generated from taskbar\assets\logo.png by taskbar\assets\make_icon.py
WizardImageFile=wizard-100.bmp,wizard-150.bmp,wizard-200.bmp
WizardSmallImageFile=wizard-small-100.bmp,wizard-small-150.bmp,wizard-small-200.bmp
UninstallDisplayIcon={app}\{#AppExe}
UninstallDisplayName={#AppName}
OutputDir=Output
OutputBaseFilename=Docksy-Setup-{#AppVersion}-arm64
WizardStyle=modern
Compression=lzma2
SolidCompression=yes
CloseApplications=no
MinVersion=10.0.22000

[Tasks]
Name: "startup"; Description: "Start Docksy when I sign in"; GroupDescription: "Startup:"

[Dirs]
Name: "{app}\hook"; Permissions: users-modify

[Files]
Source: "{#BuildDir}\{#AppExe}"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#BuildDir}\docksy_trayhook.dll"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\LICENSE"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\README.md"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\{#AppName}"; Filename: "{app}\{#AppExe}"; Comment: "Big-icon taskbar"

[Registry]
; same entry Docksy's own "Start with Windows" menu item uses
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueType: string; ValueName: "{#AppName}"; ValueData: """{app}\{#AppExe}"""; Tasks: startup; Flags: uninsdeletevalue

[Run]
Filename: "{app}\{#AppExe}"; Description: "Start Docksy now"; Flags: nowait postinstall runasoriginaluser

[UninstallRun]
; close it cleanly: it gives the screen edge back to the Windows taskbar
Filename: "{app}\{#AppExe}"; Parameters: "--quit"; RunOnceId: "QuitDocksy"; Flags: runhidden waituntilterminated
Filename: "{app}\{#AppExe}"; Parameters: "--restore"; RunOnceId: "RestoreTaskbar"; Flags: runhidden waituntilterminated

[Code]
const
  MOVEFILE_DELAY_UNTIL_REBOOT = 4;

function MoveFileEx(Existing: String; New: Cardinal; Flags: Cardinal): Boolean;
  external 'MoveFileExW@kernel32.dll stdcall';

{ Before installing over a running Docksy: close it. }
function PrepareToInstall(var NeedsRestart: Boolean): String;
var
  Code: Integer;
begin
  if FileExists(ExpandConstant('{app}\{#AppExe}')) then
    Exec(ExpandConstant('{app}\{#AppExe}'), '--quit', '', SW_HIDE, ewWaitUntilTerminated, Code);
  Result := '';
end;

{ Explorer keeps loaded hook DLL copies locked until it restarts: delete what can
  be deleted now and the rest at the next reboot. }
procedure RemoveHookFolder();
var
  Dir: String;
  FindRec: TFindRec;
begin
  Dir := ExpandConstant('{app}\hook');
  if FindFirst(Dir + '\*', FindRec) then
  begin
    try
      repeat
        if (FindRec.Attributes and FILE_ATTRIBUTE_DIRECTORY) = 0 then
          if not DeleteFile(Dir + '\' + FindRec.Name) then
            MoveFileEx(Dir + '\' + FindRec.Name, 0, MOVEFILE_DELAY_UNTIL_REBOOT);
      until not FindNext(FindRec);
    finally
      FindClose(FindRec);
    end;
  end;
  if not RemoveDir(Dir) then
    MoveFileEx(Dir, 0, MOVEFILE_DELAY_UNTIL_REBOOT);
  if not RemoveDir(ExpandConstant('{app}')) then
    MoveFileEx(ExpandConstant('{app}'), 0, MOVEFILE_DELAY_UNTIL_REBOOT);
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
var
  Data: String;
begin
  if CurUninstallStep = usUninstall then
    { the app may have added it itself (Start with Windows menu item) }
    RegDeleteValue(HKEY_CURRENT_USER, 'Software\Microsoft\Windows\CurrentVersion\Run', '{#AppName}');
  if CurUninstallStep = usPostUninstall then
  begin
    RemoveHookFolder();
    Data := ExpandConstant('{userappdata}\{#AppName}');
    if DirExists(Data) and not UninstallSilent() then
      if MsgBox('Keep your Docksy settings and pinned apps?' + #13#10 + #13#10 +
                'Yes: keep them (' + Data + '), for example to reinstall later.' + #13#10 +
                'No: delete them.', mbConfirmation, MB_YESNO) = IDNO then
        DelTree(Data, True, True, True);
  end;
end;
