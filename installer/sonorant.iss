; Inno Setup 6 script for Sonorant. Build with installer\build.ps1 (or compile this file in the IDE
; after `cargo build --release`). The models are downloaded on first use.

#define AppName    "Sonorant"
; build.ps1 passes the version from Cargo.toml (ISCC /DAppVersion=x.y.z); this default is for
; compiling the script by hand.
#ifndef AppVersion
  #define AppVersion "0.2.0"
#endif
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
; Per-user installs only: an all-users install runs elevated, so HKCU would be the administrator's
; registry rather than the signed-in user's. Each user can still turn it on in the app's settings.
Name: "startup"; Description: "Start {#AppName} when I sign in"; Flags: unchecked; Check: not IsAdminInstallMode

[InstallDelete]
; The speech engines' DLLs are matched by wildcard below; remove the old ones first so a backend
; module that a newer version renamed or dropped is not left behind (and loaded) after an upgrade.
Type: files; Name: "{app}\ggml*.dll"
Type: files; Name: "{app}\transcribe*.dll"
Type: files; Name: "{app}\onnxruntime*.dll"
Type: files; Name: "{app}\libneedle*.dll"

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
const
  RunKey = 'Software\Microsoft\Windows\CurrentVersion\Run';

// Stop Sonorant if it is still running from this installation (normally the AppMutex prompt has
// already asked the user to close it). Other copies, such as development builds, are left alone.
procedure StopInstalledApp;
var
  Exe, Params: String;
  ResultCode: Integer;
begin
  Exe := ExpandConstant('{app}\{#AppExe}');
  StringChangeEx(Exe, '''', '''''', True); // quote for a PowerShell single-quoted string
  Params := '-NoProfile -NonInteractive -ExecutionPolicy Bypass -Command "' +
    'Get-Process -Name ''' + ChangeFileExt('{#AppExe}', '') + ''' -ErrorAction SilentlyContinue | ' +
    'Where-Object { $_.Path -eq ''' + Exe + ''' } | Stop-Process -Force; Start-Sleep -Milliseconds 500"';
  Exec(ExpandConstant('{sys}\WindowsPowerShell\v1.0\powershell.exe'), Params, '', SW_HIDE,
    ewWaitUntilTerminated, ResultCode);
end;

// The Hugging Face cache, resolved as the app does (src/models/hub.rs): HF_HUB_CACHE, else
// HF_HOME\hub, else XDG_CACHE_HOME\huggingface\hub, else %USERPROFILE%\.cache\huggingface\hub.
// Empty if it cannot be determined.
function HubCacheDir: String;
begin
  Result := GetEnv('HF_HUB_CACHE');
  if Result = '' then
    Result := GetEnv('HUGGINGFACE_HUB_CACHE');
  if (Result = '') and (GetEnv('HF_HOME') <> '') then
    Result := AddBackslash(GetEnv('HF_HOME')) + 'hub';
  if (Result = '') and (GetEnv('XDG_CACHE_HOME') <> '') then
    Result := AddBackslash(GetEnv('XDG_CACHE_HOME')) + 'huggingface\hub';
  if (Result = '') and (GetEnv('USERPROFILE') <> '') then
    Result := AddBackslash(GetEnv('USERPROFILE')) + '.cache\huggingface\hub';
end;

// Delete one model repository (models--<org>--<name>) from the Hugging Face cache.
procedure DeleteCachedRepo(const Cache, Repo: String);
var
  Name: String;
begin
  Name := 'models--' + Repo;
  StringChangeEx(Name, '/', '--', True);
  DelTree(AddBackslash(Cache) + Name, True, True, True);
  DelTree(AddBackslash(Cache) + '.locks\' + Name, True, True, True);
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
var
  Cache: String;
begin
  if CurUninstallStep = usUninstall then
    StopInstalledApp
  else if CurUninstallStep = usPostUninstall then
  begin
    // Remove autostart even if it was enabled from inside the app. Only the current user's value
    // can be reached (for an all-users uninstall, the user who runs it).
    RegDeleteValue(HKCU, RunKey, '{#AppName}');
    // Settings, log and downloaded models: keep unless the user agrees. The models live in the
    // Hugging Face cache, which other programs may share: only Sonorant's model folders go.
    Cache := HubCacheDir;
    if SuppressibleMsgBox('Also delete Sonorant''s settings and the speech models it downloaded?' + #13#10#13#10 +
         'The models are stored in the Hugging Face cache (' + Cache + '), which other programs may share. ' +
         'Only the folders of the models Sonorant uses are removed; another program that uses the same ' +
         'models would download them again.', mbConfirmation, MB_YESNO or MB_DEFBUTTON2, IDNO) = IDYES then
    begin
      DelTree(ExpandConstant('{localappdata}\{#AppName}'), True, True, True);
      DelTree(ExpandConstant('{userappdata}\{#AppName}'), True, True, True);
      if Cache <> '' then
      begin
        // Keep in sync with the repositories in src/models/mod.rs, src/stt and src/config.rs.
        DeleteCachedRepo(Cache, 'onnx-community/Kokoro-82M-v1.0-ONNX');
        DeleteCachedRepo(Cache, 'onnx-community/silero-vad');
        DeleteCachedRepo(Cache, 'Cactus-Compute/whistle');
        DeleteCachedRepo(Cache, 'handy-computer/parakeet-unified-en-0.6b-gguf');
        DeleteCachedRepo(Cache, 'handy-computer/canary-180m-flash-gguf');
      end;
    end;
  end;
end;
