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
;   so the relaunch is in CurStepChanged instead), as the original user so
;   Snag doesn't inherit Setup's RedirectionGuard (see CurStepChanged).

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
; Through Explorer, like a double-click: started by Setup itself, Snag would
; inherit its RedirectionGuard (see StartSnag in [Code]).
Filename: "{win}\explorer.exe"; Parameters: """{app}\snag.exe"""; Description: "Start Snag now"; Flags: nowait postinstall skipifsilent
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

function CreateFile(lpFileName: String; dwDesiredAccess, dwShareMode, lpSecurityAttributes,
  dwCreationDisposition, dwFlagsAndAttributes, hTemplateFile: Integer): Integer;
  external 'CreateFileW@kernel32.dll stdcall';
function CloseHandle(hObject: Integer): Boolean;
  external 'CloseHandle@kernel32.dll stdcall';

var
  FfmpegChecked: Boolean;
  FfmpegAt: String;

{ "%LOCALAPPDATA%\..." -> the real folder (PATH values in the registry
  hold such names). }
function ExpandEnv(const S: String): String;
var
  N: DWORD;
begin
  N := ExpandEnvironmentStrings(S, '', 0);
  if N = 0 then
  begin
    Result := S;
    Exit;
  end;
  Result := StringOfChar(' ', N);
  N := ExpandEnvironmentStrings(S, Result, N);
  if N = 0 then
    Result := S
  else
    SetLength(Result, N - 1);
end;

{ The file exists and, if it is a link, so does what it points to.
  FileExists looks at a link itself, so a link left pointing at nothing by
  a half-finished winget upgrade counted as ffmpeg; CreateFile follows it.
  Error 448 (untrusted mount point): Setup's own RedirectionGuard won't
  follow a link a normal user made (winget's are), but Snag, started as the
  original user, can: that counts as there. }
function FileWorks(const Name: String): Boolean;
var
  H: Integer;
begin
  { No access needed, share everything, OPEN_EXISTING. }
  H := CreateFile(Name, 0, 7, 0, 3, 0, 0);
  Result := H <> -1;
  if Result then
    CloseHandle(H)
  else if DLLGetLastError = 448 then
  begin
    Log('ffmpeg: ' + Name + ' is a link Setup may not follow (RedirectionGuard); counting it as there');
    Result := True;
  end;
end;

{ Where Snag itself looks (paths::find_tool), ';'-separated: a bin folder
  next to the app (portable builds), winget's links, this process's PATH
  and the PATH saved in the registry. (The inherited PATH alone can be
  stale: an update then "installed" ffmpeg again, which upgraded it while
  Snag was already starting.) }
function FfmpegDirs: String;
var
  P: String;
begin
  Result := ExpandConstant('{app}\bin;{localappdata}\Microsoft\WinGet\Links;') + GetEnv('PATH');
  if RegQueryStringValue(HKCU, 'Environment', 'Path', P) then
    Result := Result + ';' + P;
  if RegQueryStringValue(HKLM, 'SYSTEM\CurrentControlSet\Control\Session Manager\Environment', 'Path', P) then
    Result := Result + ';' + P;
  Result := ExpandEnv(Result);
end;

{ The first working ffmpeg.exe in Dirs, or ''. Logs the broken links it skips. }
function FindFfmpegIn(Dirs: String): String;
var
  Dir, F: String;
  I: Integer;
begin
  Result := '';
  Dirs := Dirs + ';';
  while (Result = '') and (Dirs <> '') do
  begin
    I := Pos(';', Dirs);
    Dir := Trim(Copy(Dirs, 1, I - 1));
    Delete(Dirs, 1, I);
    if Dir <> '' then
    begin
      F := AddBackslash(Dir) + 'ffmpeg.exe';
      if FileExists(F) then
        if FileWorks(F) then
          Result := F
        else
          Log('ffmpeg: skipping ' + F + ': a broken link (what it points to is gone)');
    end;
  end;
end;

function FindFfmpeg: String;
begin
  Result := FindFfmpegIn(FfmpegDirs);
end;

{ Checked once (Inno asks a task's Check several times); the result goes to
  the log, which Snag's updater asks for (/LOG). }
function FfmpegMissing: Boolean;
begin
  if not FfmpegChecked then
  begin
    FfmpegChecked := True;
    FfmpegAt := FindFfmpeg;
    if FfmpegAt <> '' then
      Log('ffmpeg: found ' + FfmpegAt)
    else
      Log('ffmpeg: not found in ' + FfmpegDirs);
  end;
  Result := FfmpegAt = '';
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

{ Starts Snag the way a double-click does: through Explorer. Whatever Setup
  starts itself (Exec, and even ExecAsOriginalUser when Setup isn't
  elevated) inherits Setup's RedirectionGuard: Snag then can't follow
  winget's ffmpeg links (made without admin rights), gets error 448 and
  finds no ffmpeg until it is restarted by hand. The explorer.exe started
  here hands the file to the running Explorer, which starts Snag. }
procedure StartSnag;
var
  Code: Integer;
begin
  Log('starting Snag again');
  if not Exec(ExpandConstant('{win}\explorer.exe'), AddQuotes(ExpandConstant('{app}\snag.exe')), '', SW_SHOWNORMAL, ewNoWait, Code) then
  begin
    Log('explorer.exe could not be started (' + SysErrorMessage(Code) + '); starting Snag directly');
    Exec(ExpandConstant('{app}\snag.exe'), '', ExpandConstant('{app}'), SW_SHOWNORMAL, ewNoWait, Code);
  end;
end;

procedure CurStepChanged(CurStep: TSetupStep);
var
  Code: Integer;
  Started: Boolean;
  After: String;
begin
  if (CurStep = ssPostInstall) and WizardIsTaskSelected('ffmpeg') then
  begin
    WizardForm.StatusLabel.Caption := 'Installing ffmpeg with winget (this can take a minute)...';
    Started := Exec('winget', 'install --id Gyan.FFmpeg -e --silent --accept-package-agreements --accept-source-agreements',
                    '', SW_HIDE, ewWaitUntilTerminated, Code);
    if Started then
      Log('winget finished with code ' + IntToStr(Code))
    else
      Log('winget could not be started: ' + SysErrorMessage(Code));
    After := FindFfmpeg;
    if After <> '' then
      Log('ffmpeg after winget: found ' + After)
    else
      Log('ffmpeg after winget: still not found');
    if not Started or ((Code <> 0) and (Code <> -1978335189)) then
      MsgBox('ffmpeg could not be installed automatically (winget code ' + IntToStr(Code) + ').' + #13#10#13#10 +
             'Open PowerShell or Terminal and run:' + #13#10 + '    winget install Gyan.FFmpeg' + #13#10 +
             'Snag starts recording by itself once it is installed.', mbInformation, MB_OK);
  end;
  { After an update from inside Snag: start it again, now that ffmpeg is in
    place (winget above has finished). }
  if (CurStep = ssPostInstall) and Relaunch then
    StartSnag;
end;
