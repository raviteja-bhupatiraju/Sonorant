; Inno Setup 6 script for Sonorant. Build with installer\build.ps1 (or compile this file in the IDE
; after `cargo build --release`). The models are downloaded on first use.

#define AppName    "Sonorant"
#define AppVersion "0.1.0"
#define AppExe     "sonorant.exe"
#define BuildDir   "..\target\release"

[Setup]
; Never change AppId: it identifies this app for upgrades and uninstall.
AppId={{B3C1F0A2-6D4E-4F5B-9A7E-2C8D1E5F4A63}
AppName={#AppName}
AppVersion={#AppVersion}
AppVerName={#AppName} {#AppVersion}
AppPublisher=Ravi Teja Bhupatiraju
AppCopyright=Copyright (c) 2026 Ravi Teja Bhupatiraju (MIT License)
VersionInfoVersion={#AppVersion}
DefaultDirName={autopf}\{#AppName}
DisableProgramGroupPage=yes
DisableReadyPage=yes
; Per-user install by default (no UAC); the user may elevate for all users from the dialog.
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0
; Setup asks the user to close Sonorant if it is running (the app holds this mutex).
AppMutex=Local\Sonorant.SingleInstance
UninstallDisplayIcon={app}\{#AppExe}
OutputDir=output
OutputBaseFilename={#AppName}-Setup-{#AppVersion}
Compression=lzma2/ultra64
SolidCompression=yes
WizardStyle=modern

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; Flags: unchecked
Name: "startup"; Description: "Start {#AppName} when I sign in"; Flags: unchecked

[Files]
Source: "{#BuildDir}\{#AppExe}"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\LICENSE"; DestDir: "{app}"; DestName: "LICENSE.txt"; Flags: ignoreversion
Source: "{#BuildDir}\onnxruntime.dll"; DestDir: "{app}"; Flags: ignoreversion
; Cactus Whistle speech recognition engine (Needle), loaded at run time.
Source: "{#BuildDir}\libneedle3.dll"; DestDir: "{app}"; Flags: ignoreversion
; Parakeet / Canary speech recognition engine (transcribe.cpp / ggml): the Vulkan GPU module and the CPU
; variants are loaded at run time; the best one for the machine is chosen automatically (CPU if there is no GPU).
Source: "{#BuildDir}\transcribe.dll"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#BuildDir}\ggml*.dll"; DestDir: "{app}"; Flags: ignoreversion
; Visual C++ runtime, app-local (staged by build.ps1) so a clean Windows needs no redistributable.
Source: "redist\*.dll"; DestDir: "{app}"; Flags: ignoreversion
; eSpeak-ng phoneme data; staged next to the exe by build.ps1. The app looks for it beside sonorant.exe.
Source: "{#BuildDir}\espeak-ng-data\*"; DestDir: "{app}\espeak-ng-data"; Flags: ignoreversion recursesubdirs createallsubdirs

[Icons]
Name: "{autoprograms}\{#AppName}"; Filename: "{app}\{#AppExe}"
Name: "{autodesktop}\{#AppName}"; Filename: "{app}\{#AppExe}"; Tasks: desktopicon

[Registry]
; Same value the app's own "Start with Windows" option writes.
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueType: string; ValueName: "{#AppName}"; \
  ValueData: """{app}\{#AppExe}"""; Tasks: startup; Flags: uninsdeletevalue

[Run]
Filename: "{app}\{#AppExe}"; Description: "Launch {#AppName}"; Flags: nowait postinstall skipifsilent

[Code]
procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
var
  ResultCode: Integer;
begin
  if CurUninstallStep = usUninstall then
    Exec(ExpandConstant('{sys}\taskkill.exe'), '/F /IM {#AppExe}', '', SW_HIDE, ewWaitUntilTerminated, ResultCode)
  else if CurUninstallStep = usPostUninstall then
  begin
    // Remove autostart even if it was enabled from inside the app.
    RegDeleteValue(HKCU, 'Software\Microsoft\Windows\CurrentVersion\Run', '{#AppName}');
    // Downloaded models (~330 MB), settings and log: keep unless the user agrees.
    if SuppressibleMsgBox('Also delete Sonorant settings and downloaded voice models?', mbConfirmation,
         MB_YESNO or MB_DEFBUTTON2, IDNO) = IDYES then
    begin
      DelTree(ExpandConstant('{localappdata}\{#AppName}'), True, True, True);
      DelTree(ExpandConstant('{userappdata}\{#AppName}'), True, True, True);
    end;
  end;
end;
