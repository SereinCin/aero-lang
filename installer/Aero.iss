; Aero 1.1.0 — Windows Installer
; InnoSetup script. Build: ISCC Aero.iss

#define MyAppName "Aero"
#define MyAppVersion "1.1.0"
#define MyAppPublisher "Aero Project"
#define MyAppURL "https://github.com/aero-lang/aero"
#define MyAppExeName "aero.exe"

[Setup]
AppId={{B8F4C3A1-2D5E-4A7F-9B6C-8D1E3F2A5C7B}
AppName={#MyAppName}
AppVersion={#MyAppVersion}
AppPublisher={#MyAppPublisher}
AppPublisherURL={#MyAppURL}
AppSupportURL={#MyAppURL}
AppUpdatesURL={#MyAppURL}
DefaultDirName={autopf}\{#MyAppName}
DefaultGroupName={#MyAppName}
DisableProgramGroupPage=yes
LicenseFile=
InfoBeforeFile=
OutputDir=..\
OutputBaseFilename=Aero-{#MyAppVersion}-windows-x64
Compression=lzma
SolidCompression=yes
WizardStyle=modern
PrivilegesRequired=admin
PrivilegesRequiredOverridesAllowed=dialog
SetupIconFile=
UninstallDisplayIcon={app}\bin\{#MyAppExeName}
UninstallDisplayName={#MyAppName} {#MyAppVersion}
ChangesEnvironment=yes

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "addtopath"; Description: "Add Aero to PATH (recommended)"; GroupDescription: "Environment:"
Name: "addtopath_system"; Description: "System-wide PATH (all users)"; GroupDescription: "Environment:"; Flags: exclusive
Name: "addtopath_user"; Description: "Current user PATH only"; GroupDescription: "Environment:"; Flags: exclusive
Name: "associate_aero"; Description: "Associate .aero files with Aero"; GroupDescription: "File associations:"
Name: "desktopicon"; Description: "Create a desktop shortcut"; GroupDescription: "Shortcuts:"; Flags: checkedonce

[Files]
Source: "files\{#MyAppExeName}"; DestDir: "{app}\bin"; Flags: ignoreversion
Source: "files\aero_cmd.bat"; DestDir: "{app}\bin"; Flags: ignoreversion
Source: "files\aero_env.bat"; DestDir: "{app}\bin"; Flags: ignoreversion

[Icons]
Name: "{group}\Aero Command Prompt"; Filename: "{app}\bin\aero_cmd.bat"; WorkingDir: "{app}"; Comment: "Open a command prompt with Aero environment"
Name: "{group}\Aero (uninstall)"; Filename: "{uninstallexe}"
Name: "{commondesktop}\Aero Command Prompt"; Filename: "{app}\bin\aero_cmd.bat"; WorkingDir: "{app}"; Tasks: desktopicon; Comment: "Open a command prompt with Aero environment"

[Registry]
; .aero file association
Root: HKCR; Subkey: ".aero"; ValueType: string; ValueName: ""; ValueData: "AeroSourceFile"; Flags: uninsdeletevalue; Tasks: associate_aero
Root: HKCR; Subkey: "AeroSourceFile"; ValueType: string; ValueName: ""; ValueData: "Aero Source File"; Flags: uninsdeletekey; Tasks: associate_aero
Root: HKCR; Subkey: "AeroSourceFile\DefaultIcon"; ValueType: string; ValueName: ""; ValueData: "{app}\bin\{#MyAppExeName},0"; Tasks: associate_aero
Root: HKCR; Subkey: "AeroSourceFile\shell\open\command"; ValueType: string; ValueName: ""; ValueData: """{app}\bin\{#MyAppExeName}"" run ""%1"""; Tasks: associate_aero
; PATH (system-wide)
Root: HKLM; Subkey: "SYSTEM\CurrentControlSet\Control\Session Manager\Environment"; ValueType: expandsz; ValueName: "Path"; ValueData: "{olddata};{app}\bin"; Check: IsAdmin and IsTaskSelected('addtopath_system'); Tasks: addtopath_system
; PATH (user)
Root: HKCU; Subkey: "Environment"; ValueType: expandsz; ValueName: "Path"; ValueData: "{olddata};{app}\bin"; Check: not IsAdmin or IsTaskSelected('addtopath_user'); Tasks: addtopath_user

[Run]
Filename: "{app}\bin\aero_cmd.bat"; Description: "Launch Aero Command Prompt"; Flags: postinstall nowait skipifsilent shellexec; WorkingDir: "{app}"

[Code]
// Check if user selected any PATH task
function IsTaskSelected(const TaskName: string): Boolean;
begin
  Result := WizardIsTaskSelected(TaskName);
end;

function IsAdmin: Boolean;
begin
  Result := IsAdminInstallMode or IsPowerUserLoggedOn;
end;

procedure CurStepChanged(CurStep: TSetupStep);
begin
  if CurStep = ssPostInstall then
  begin
    // Notify about environment changes
    if IsTaskSelected('addtopath_system') or IsTaskSelected('addtopath_user') then
    begin
      MsgBox('Aero has been added to PATH. You may need to restart CMD or log out/in for the change to take effect.', mbInformation, MB_OK);
    end;
  end;
end;