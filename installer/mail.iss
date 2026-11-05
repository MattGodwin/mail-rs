; Inno Setup script for the Windows installer.
; Built by .github/workflows/release.yml:
;   iscc /DAppVersion=0.2.0 /DSourceExe=path\to\mail-rs.exe installer\mail.iss
;
; Per-user install (no administrator prompt) into %LOCALAPPDATA%\Programs\Mail,
; so the in-app updater can update it later without elevation.

#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif
#ifndef SourceExe
  #define SourceExe "..\target\release\mail-rs.exe"
#endif
#ifndef OutputDir
  #define OutputDir "..\dist"
#endif

[Setup]
; Never change AppId: it ties upgrades and the uninstaller together.
AppId={{8E5C2A61-3F4B-4C7E-9A1D-6B2F0C9E7D41}
AppName=Mail
AppVersion={#AppVersion}
AppVerName=Mail {#AppVersion}
AppPublisher=Matthew Godwin
AppPublisherURL=https://github.com/MattGodwin/mail-rs
AppSupportURL=https://github.com/MattGodwin/mail-rs/issues
AppUpdatesURL=https://github.com/MattGodwin/mail-rs/releases
DefaultDirName={localappdata}\Programs\Mail
DisableDirPage=auto
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
OutputDir={#OutputDir}
OutputBaseFilename=mail-rs-setup-windows-x64
SetupIconFile=..\assets\icon.ico
UninstallDisplayIcon={app}\mail-rs.exe
UninstallDisplayName=Mail
LicenseFile=..\LICENSE
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
; Close a running Mail before replacing files (used by silent updates too).
CloseApplications=yes
RestartApplications=no
VersionInfoVersion={#AppVersion}
VersionInfoProductName=Mail

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
Source: "{#SourceExe}"; DestDir: "{app}"; DestName: "mail-rs.exe"; Flags: ignoreversion
Source: "..\README.md"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\LICENSE"; DestDir: "{app}"; DestName: "LICENSE.txt"; Flags: ignoreversion

[Icons]
; AppUserModelID gives notifications their own name and icon (see src/notify.rs).
Name: "{autoprograms}\Mail"; Filename: "{app}\mail-rs.exe"; AppUserModelID: "MattGodwin.Mail"
Name: "{autodesktop}\Mail"; Filename: "{app}\mail-rs.exe"; Tasks: desktopicon

[Run]
; Shown as "Launch Mail" after an interactive install; also reopens Mail after
; a silent in-app update unless the app asked for /NORELAUNCH (closing Mail).
Filename: "{app}\mail-rs.exe"; Description: "{cm:LaunchProgram,Mail}"; Flags: nowait postinstall; Check: ShouldLaunch

; Mail's settings, cache and saved credentials are deliberately left in place on
; uninstall, so reinstalling picks up where you left off.

[Code]
function ShouldLaunch: Boolean;
begin
  Result := Pos('/NORELAUNCH', UpperCase(GetCmdTail)) = 0;
end;
