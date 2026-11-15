; Inno Setup 6 script for the nit installer: per user, no admin rights.
;   ISCC /DAppVersion=0.1.0 [/DSourceExe=path\to\nit.exe] [/DOutputDir=dir] packaging\windows\nit.iss
; Output: nit-<AppVersion>-windows-x86_64-setup.exe

#ifndef AppVersion
  #error Pass the version: ISCC /DAppVersion=x.y.z nit.iss
#endif
#define RepoRoot AddBackslash(SourcePath) + "..\.."
#ifndef SourceExe
  #define SourceExe RepoRoot + "\target\release\nit.exe"
#endif
#ifndef OutputDir
  #define OutputDir RepoRoot + "\dist-out"
#endif
; Windows version resources take numbers only: 1.2.0-rc.1 -> 1.2.0
#define NumericVersion Copy(AppVersion, 1, Pos("-", AppVersion + "-") - 1)

[Setup]
; Never change the AppId: upgrades and the uninstaller find the installed copy by it.
AppId={{47FB3ABD-6D10-4A23-A463-1923D94F4D00}
AppName=nit
AppVersion={#AppVersion}
AppVerName=nit {#AppVersion}
AppPublisher=The nit contributors
AppPublisherURL=https://github.com/Scorpse/nit
AppSupportURL=https://github.com/Scorpse/nit/issues
AppUpdatesURL=https://github.com/Scorpse/nit/releases
VersionInfoVersion={#NumericVersion}
VersionInfoProductName=nit
VersionInfoDescription=nit setup
; per-user install: {autopf} is %LOCALAPPDATA%\Programs, shortcuts go to the user's Start Menu
PrivilegesRequired=lowest
DefaultDirName={autopf}\nit
DisableProgramGroupPage=yes
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0
OutputDir={#OutputDir}
OutputBaseFilename=nit-{#AppVersion}-windows-x86_64-setup
SetupIconFile={#RepoRoot}\packaging\icons\nit.ico
UninstallDisplayIcon={app}\nit.exe
UninstallDisplayName=nit
WizardStyle=modern
Compression=lzma2/max
SolidCompression=yes
; a running nit is closed (Restart Manager) before its files are replaced, and not restarted
CloseApplications=force
RestartApplications=no
; the optional PATH entry
ChangesEnvironment=yes

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked
Name: "addtopath"; Description: "Add nit to the PATH (to run nit --check-token in a terminal)"; GroupDescription: "Other:"; Flags: unchecked

[Files]
Source: "{#SourceExe}"; DestDir: "{app}"; DestName: "nit.exe"; Flags: ignoreversion
Source: "{#RepoRoot}\README.md"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#RepoRoot}\LICENSE-MIT"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#RepoRoot}\LICENSE-APACHE"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#RepoRoot}\THIRD-PARTY-NOTICES.md"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\nit"; Filename: "{app}\nit.exe"; Comment: "Pull request review"
Name: "{autodesktop}\nit"; Filename: "{app}\nit.exe"; Comment: "Pull request review"; Tasks: desktopicon

[Run]
Filename: "{app}\nit.exe"; Description: "{cm:LaunchProgram,nit}"; Flags: nowait postinstall skipifsilent

[UninstallRun]
; close a running nit so its files can be removed (drafts are saved as they are typed)
Filename: "{sys}\taskkill.exe"; Parameters: "/F /IM nit.exe"; Flags: runhidden; RunOnceId: "StopNit"

[Code]
const
  EnvKey = 'Environment';

function PathWithout(Path, Dir: String): String;
var
  Rest, Part: String;
  I: Integer;
begin
  Result := '';
  Rest := Path + ';';
  while Rest <> '' do
  begin
    I := Pos(';', Rest);
    Part := Copy(Rest, 1, I - 1);
    Delete(Rest, 1, I);
    if (Part <> '') and (CompareText(RemoveBackslashUnlessRoot(Part), RemoveBackslashUnlessRoot(Dir)) <> 0) then
    begin
      if Result <> '' then
        Result := Result + ';';
      Result := Result + Part;
    end;
  end;
end;

procedure SetUserPath(Add: Boolean);
var
  Path, NewPath, Dir: String;
begin
  Dir := ExpandConstant('{app}');
  if not RegQueryStringValue(HKCU, EnvKey, 'Path', Path) then
    Path := '';
  NewPath := PathWithout(Path, Dir);
  if Add then
  begin
    if NewPath <> '' then
      NewPath := NewPath + ';';
    NewPath := NewPath + Dir;
  end;
  if NewPath <> Path then
    RegWriteExpandStringValue(HKCU, EnvKey, 'Path', NewPath);
end;

procedure CurStepChanged(CurStep: TSetupStep);
begin
  if CurStep = ssPostInstall then
    SetUserPath(WizardIsTaskSelected('addtopath'));
end;

// Deletes the sign-ins nit saved in Windows Credential Manager (targets
// account.<id>.email.nit / account.<id>.token.nit, and bitbucket.*.nit of older versions).
procedure ForgetSignIns;
var
  Params: String;
  ResultCode: Integer;
begin
  Params := '-NoProfile -NonInteractive -ExecutionPolicy Bypass -Command "' +
    'cmdkey /list | ForEach-Object { ' +
    'if ($_ -match ''\s(\S*(account\.\S+|bitbucket)\.(email|token)\.nit)\s*$'') ' +
    '{ cmdkey /delete:$($Matches[1]) | Out-Null } }"';
  Exec(ExpandConstant('{sys}\WindowsPowerShell\v1.0\powershell.exe'), Params, '', SW_HIDE,
    ewWaitUntilTerminated, ResultCode);
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
var
  DataDir: String;
begin
  if CurUninstallStep = usUninstall then
    SetUserPath(False);
  if CurUninstallStep = usPostUninstall then
  begin
    DataDir := ExpandConstant('{localappdata}\nit');
    // default (and the answer of a silent uninstall): keep everything
    if SuppressibleMsgBox('Also delete your nit data?' + #13#10#13#10 +
      'This removes the cache, drafts, logs and git mirrors in ' + DataDir +
      ', and the sign-ins nit saved in Windows Credential Manager.' + #13#10 +
      'Mirrors in a download folder you chose yourself are kept.' + #13#10#13#10 +
      'Choose No to keep them for a later install.',
      mbConfirmation, MB_YESNO or MB_DEFBUTTON2, IDNO) = IDYES then
    begin
      DelTree(DataDir, True, True, True);
      ForgetSignIns;
    end;
  end;
end;
