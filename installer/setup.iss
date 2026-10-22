; Inno Setup script for Simple Converter. Build with installer\build.ps1.

#define AppName "Simple Converter"
#define AppExe "simple-converter.exe"
#define AppUrl "https://github.com/rostislav-kuparo/simple-converter"
#ifndef AppVersion
  #define AppVersion "0.2.0"
#endif
; Folder with ffmpeg.exe and ffprobe.exe; LICENSE/README of the build are taken from its parent.
#ifndef FFmpegBin
  #define FFmpegBin "C:\ffmpeg\bin"
#endif

[Setup]
AppId={{2FF5BB08-C47A-4852-BA00-93F46A792D67}
AppName={#AppName}
AppVersion={#AppVersion}
AppVerName={#AppName} {#AppVersion}
AppPublisher=Kuparo
AppPublisherURL={#AppUrl}
AppSupportURL={#AppUrl}/issues
AppUpdatesURL={#AppUrl}/releases
DefaultDirName={autopf}\{#AppName}
DefaultGroupName={#AppName}
DisableProgramGroupPage=yes
; Installs for the current user without an admin prompt; admins can still pick "all users".
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0
OutputDir=..\dist
OutputBaseFilename=SimpleConverter-Setup-{#AppVersion}
SetupIconFile=..\assets\icon.ico
UninstallDisplayIcon={app}\{#AppExe}
UninstallDisplayName={#AppName}
WizardStyle=modern
CloseApplications=yes
RestartApplications=no
VersionInfoVersion={#AppVersion}
VersionInfoProductName={#AppName}
; ffmpeg.exe and ffprobe.exe are ~95% identical: a dictionary larger than one of them
; lets LZMA store the second one almost for free.
Compression=lzma2/ultra64
SolidCompression=yes
LZMAUseSeparateProcess=yes
LZMADictionarySize=262144
LZMANumFastBytes=273

[Languages]
Name: "ru"; MessagesFile: "compiler:Languages\Russian.isl"
Name: "en"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"

[Files]
Source: "..\target\release\{#AppExe}"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\LICENSE"; DestDir: "{app}"; DestName: "LICENSE.txt"; Flags: ignoreversion
Source: "{#FFmpegBin}\ffmpeg.exe"; DestDir: "{app}\ffmpeg"; Flags: ignoreversion
Source: "{#FFmpegBin}\ffprobe.exe"; DestDir: "{app}\ffmpeg"; Flags: ignoreversion
Source: "FFMPEG-NOTICE.txt"; DestDir: "{app}\ffmpeg"; Flags: ignoreversion
Source: "{#FFmpegBin}\..\LICENSE"; DestDir: "{app}\ffmpeg"; DestName: "LICENSE.txt"; Flags: ignoreversion skipifsourcedoesntexist
Source: "{#FFmpegBin}\..\README.txt"; DestDir: "{app}\ffmpeg"; Flags: ignoreversion skipifsourcedoesntexist

[Icons]
Name: "{autoprograms}\{#AppName}"; Filename: "{app}\{#AppExe}"
Name: "{autodesktop}\{#AppName}"; Filename: "{app}\{#AppExe}"; Tasks: desktopicon

[Run]
Filename: "{app}\{#AppExe}"; Description: "{cm:LaunchProgram,{#AppName}}"; Flags: nowait postinstall skipifsilent

[UninstallRun]
Filename: "{sys}\taskkill.exe"; Parameters: "/F /IM {#AppExe}"; Flags: runhidden; RunOnceId: "StopApp"
