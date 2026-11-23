; Snag installer (Inno Setup 6). Build it with scripts\installer.ps1, which
; passes the version and the folder of files to install:
;   ISCC /DAppVersion=0.4.1 /DSourceDir=..\dist\Snag /DIconFile=... snag.iss
;
; * Installs per user (no administrator prompt) into
;   %LOCALAPPDATA%\Programs\Snag, with a Start menu shortcut, optional
;   desktop shortcut and optional "start with Windows".
; * Offers to install ffmpeg with winget when it isn't found (Snag needs
;   it to record).
; * Closes a running Snag before updating; settings (%APPDATA%\Snag) and
;   clips are kept on update and uninstall.
; * Snag's own updater runs it with /SILENT /RELAUNCH=1: it updates in
;   place and starts Snag again afterwards, after ffmpeg if that had to be
;   installed ([Run] entries run BEFORE ssPostInstall, where winget runs,
;   so the relaunch is in CurStepChanged instead).

#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif
#ifndef SourceDir
  #define SourceDir "..\dist\Snag"
#endif

[Setup]
AppId={{6B0E2C1D-7A43-4F5E-9C8B-5A1D3E2F4C70}
AppName=Snag
AppVersion={#AppVersion}
AppVerName=Snag {#AppVersion}
AppPublisher=ph4ntom
AppPublisherURL=https://github.com/ezph4ntom/Snag
AppSupportURL=https://github.com/ezph4ntom/Snag/issues
DefaultDirName={autopf}\Snag
DisableProgramGroupPage=yes
DisableDirPage=auto
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog
OutputDir=..\dist
OutputBaseFilename=Snag-Setup-{#AppVersion}
#ifdef IconFile
SetupIconFile={#IconFile}
#endif
UninstallDisplayIcon={app}\snag.exe
UninstallDisplayName=Snag
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0.17763
CloseApplications=yes
RestartApplications=no

[Tasks]
Name: "desktopicon"; Description: "Create a desktop shortcut"; GroupDescription: "Shortcuts:"
Name: "startup"; Description: "Start Snag when Windows starts"; GroupDescription: "Shortcuts:"; Flags: unchecked
Name: "ffmpeg"; Description: "Install ffmpeg (Snag needs it to record; downloads about 100 MB with winget)"; GroupDescription: "Required:"; Check: FfmpegMissing

[InstallDelete]
; Older versions shipped the fonts next to the exe; they are built in now.
Type: files; Name: "{app}\assets\fonts\*.ttf"

[Files]
Source: "{#SourceDir}\*"; DestDir: "{app}"; Flags: recursesubdirs ignoreversion

[Registry]
; The same value Snag's Settings > Startup switch reads and writes.
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueType: string; ValueName: "Snag"; ValueData: """{app}\snag.exe"" --background"; Tasks: startup; Flags: uninsdeletevalue

[Icons]
Name: "{autoprograms}\Snag"; Filename: "{app}\snag.exe"; Comment: "Game clipping"
Name: "{autodesktop}\Snag"; Filename: "{app}\snag.exe"; Tasks: desktopicon; Comment: "Game clipping"

[Run]
Filename: "{app}\snag.exe"; Description: "Start Snag now"; Flags: nowait postinstall skipifsilent
; (After an update from inside Snag, CurStepChanged starts it again.)

[UninstallRun]
; Ask a running Snag to quit (cleanly stops recording) before files go.
Filename: "{app}\snag.exe"; Parameters: "--quit"; Flags: runhidden waituntilterminated; RunOnceId: "QuitSnag"

[Code]
function Relaunch: Boolean;
begin
  Result := WizardSilent and (ExpandConstant('{param:RELAUNCH|0}') = '1');
end;

function ExpandEnvironmentStrings(lpSrc: String; lpDst: String; nSize: DWORD): DWORD;
  external 'ExpandEnvironmentStringsW@kernel32.dll stdcall';

{ "%LOCALAPPDATA%\..." -> the real folder (PATH values in the registry
  hold such names). }
function ExpandEnv(const S: String): String;
var
  N: DWORD;
begin
  Result := StringOfChar(' ', 8192);
  N := ExpandEnvironmentStrings(S, Result, 8192);
  if (N = 0) or (N > 8192) then
    Result := S
  else
    SetLength(Result, N - 1);
end;

function OnPath(const Dirs: String): Boolean;
begin
  Result := (Dirs <> '') and (FileSearch('ffmpeg.exe', ExpandEnv(Dirs)) <> '');
end;

function FfmpegMissing: Boolean;
var
  P: String;
begin
  { Where Snag itself looks (paths::find_tool): a bin folder next to the
    app (portable builds), winget's links, this process's PATH and the
    PATH saved in the registry. The inherited PATH alone can be stale
    (ffmpeg installed after Snag started): an update then "installed"
    ffmpeg again, which upgraded it while Snag was already starting. }
  Result := not (FileExists(ExpandConstant('{app}\bin\ffmpeg.exe'))
    or FileExists(ExpandConstant('{localappdata}\Microsoft\WinGet\Links\ffmpeg.exe'))
    or OnPath(GetEnv('PATH'))
    or (RegQueryStringValue(HKCU, 'Environment', 'Path', P) and OnPath(P))
    or (RegQueryStringValue(HKLM, 'SYSTEM\CurrentControlSet\Control\Session Manager\Environment', 'Path', P) and OnPath(P)));
end;

function PrepareToInstall(var NeedsRestart: Boolean): String;
var
  Code: Integer;
begin
  { Updating: ask the running Snag to quit first. }
  if FileExists(ExpandConstant('{app}\snag.exe')) then
  begin
    Exec(ExpandConstant('{app}\snag.exe'), '--quit', '', SW_HIDE, ewWaitUntilTerminated, Code);
    Sleep(2500);
  end;
  Result := '';
end;

procedure CurStepChanged(CurStep: TSetupStep);
var
  Code: Integer;
begin
  if (CurStep = ssPostInstall) and WizardIsTaskSelected('ffmpeg') then
  begin
    WizardForm.StatusLabel.Caption := 'Installing ffmpeg with winget (this can take a minute)...';
    if not Exec('winget', 'install --id Gyan.FFmpeg -e --silent --accept-package-agreements --accept-source-agreements',
                '', SW_HIDE, ewWaitUntilTerminated, Code) or ((Code <> 0) and (Code <> -1978335189)) then
      MsgBox('ffmpeg could not be installed automatically (winget code ' + IntToStr(Code) + ').' + #13#10#13#10 +
             'Open PowerShell or Terminal and run:' + #13#10 + '    winget install Gyan.FFmpeg' + #13#10 +
             'Snag starts recording by itself once it is installed.', mbInformation, MB_OK);
  end;
  { After an update from inside Snag: start it again, now that ffmpeg is in
    place (winget above has finished). }
  if (CurStep = ssPostInstall) and Relaunch then
    Exec(ExpandConstant('{app}\snag.exe'), '', ExpandConstant('{app}'), SW_SHOWNORMAL, ewNoWait, Code);
end;
