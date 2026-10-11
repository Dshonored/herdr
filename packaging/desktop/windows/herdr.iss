; Per-user installer for the herdr desktop app. Installing into the user's own
; Programs folder needs no administrator rights and keeps the folder writable, so the
; app can update itself in place.
#define AppVersion GetEnv("HERDR_DESKTOP_VERSION")
#define StageDir GetEnv("HERDR_STAGE_DIR")

[Setup]
AppId={{6F1D2C8A-3B57-4E0B-9C1A-5D2E7F4A8B91}
AppName=herdr
AppVersion={#AppVersion}
AppVerName=herdr {#AppVersion}
AppPublisher=herdr
DefaultDirName={localappdata}\Programs\herdr
DisableDirPage=yes
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
OutputDir=.
OutputBaseFilename=herdr-desktop-windows-x86_64-setup
SetupIconFile={#StageDir}\herdr.ico
UninstallDisplayIcon={app}\herdr.ico
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
CloseApplications=yes

[Tasks]
Name: desktopicon; Description: "Create a desktop shortcut"; Flags: unchecked

[Files]
Source: "{#StageDir}\*"; DestDir: "{app}"; Flags: recursesubdirs ignoreversion

[Icons]
Name: "{autoprograms}\herdr"; Filename: "{app}\herdr-desktop.exe"; IconFilename: "{app}\herdr.ico"
Name: "{autodesktop}\herdr"; Filename: "{app}\herdr-desktop.exe"; IconFilename: "{app}\herdr.ico"; Tasks: desktopicon

[Run]
Filename: "{app}\herdr-desktop.exe"; Description: "Open herdr"; Flags: nowait postinstall skipifsilent
