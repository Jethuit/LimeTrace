#define MyAppName "LimeTrace"
#define MyAppVersion "0.1.0"
#define MyAppPublisher "LimeTrace"
#define MyAppExeName "limetrace.exe"
#define MyDaemonExeName "limetrace-backend.exe"
#define MyDaemonRunValueName "LimeTraceBackend"

[Setup]
AppId={{D0A6D2FC-8F7A-4F13-8CE5-0B74F8913E71}
AppName={#MyAppName}
AppVersion={#MyAppVersion}
AppPublisher={#MyAppPublisher}
DefaultDirName={autopf}\LimeTrace
DefaultGroupName={#MyAppName}
DisableProgramGroupPage=yes
OutputDir=..\dist\installer
OutputBaseFilename=LimeTraceSetup
Compression=lzma
SolidCompression=yes
WizardStyle=modern
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
UninstallDisplayIcon={app}\{#MyAppExeName}

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
Source: "..\target\release\limetrace-backend.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\target\release\limetrace.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\README.md"; DestDir: "{app}"; DestName: "README.txt"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\{#MyAppName}\Open LimeTrace"; Filename: "{app}\{#MyAppExeName}"
Name: "{autoprograms}\{#MyAppName}\Uninstall"; Filename: "{uninstallexe}"
Name: "{autodesktop}\{#MyAppName}"; Filename: "{app}\{#MyAppExeName}"; Tasks: desktopicon

[Registry]
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueType: string; ValueName: "{#MyDaemonRunValueName}"; ValueData: """{app}\{#MyDaemonExeName}"""; Flags: uninsdeletevalue

[Run]
Filename: "{app}\{#MyDaemonExeName}"; Description: "Start LimeTrace Backend now"; Flags: postinstall nowait skipifsilent
Filename: "{app}\{#MyAppExeName}"; Description: "Open LimeTrace now"; Flags: postinstall skipifsilent

[UninstallRun]
Filename: "{cmd}"; Parameters: "/C taskkill /IM {#MyDaemonExeName} /F >nul 2>&1"; Flags: runhidden
