; Inno Setup 6 script for Explorer Session Recall.
;
; Setup itself installs nothing: it extracts esr.exe to a temporary folder and
; runs `esr.exe --install --quiet`, which does the per-user install (program
; folder, Start-menu shortcut, start at sign-in, Installed-apps entry, upgrade
; from older builds) and starts the installed copy in the background.
; Uninstalling goes through the Installed-apps entry that esr.exe writes
; (`esr.exe --uninstall`), so Setup has no uninstaller of its own.
;
; Build with installer\build.ps1 (or: ISCC /DAppVersion=x.y.z /DSourceExe=<esr.exe> ExplorerSessionRecall.iss).

#ifndef AppVersion
  #define AppVersion "1.0.0"
#endif
#ifndef SourceExe
  #define SourceExe "..\target\release\esr.exe"
#endif

#define AppName "Explorer Session Recall"
#define AppUrl "https://github.com/Ataraksea/ExplorerSessionRecall.git"
#define TroubleshootingUrl "https://github.com/Ataraksea/ExplorerSessionRecall#troubleshooting"

[Setup]
AppId={{A0F5A066-BB3F-491E-8EC2-1A83D83CBF27}
AppName={#AppName}
AppVersion={#AppVersion}
AppPublisher=Ataraksea
AppPublisherURL={#AppUrl}
AppSupportURL={#AppUrl}
AppUpdatesURL={#AppUrl}
PrivilegesRequired=lowest
Uninstallable=no
CreateAppDir=no
DisableProgramGroupPage=yes
CloseApplications=no
ShowLanguageDialog=no
LicenseFile=..\LICENSE-MIT
OutputDir=..\dist
OutputBaseFilename=ExplorerSessionRecall-Setup-{#AppVersion}
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0.22621
VersionInfoVersion={#AppVersion}
VersionInfoProductName={#AppName}
VersionInfoProductVersion={#AppVersion}
VersionInfoCompany=Ataraksea
VersionInfoDescription={#AppName} Setup
VersionInfoCopyright=Copyright (c) 2026 Ataraksea

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Files]
Source: "{#SourceExe}"; DestDir: "{tmp}"; DestName: "esr.exe"; Flags: ignoreversion

[Code]
var
  InstallFailed: Boolean;
  InstallWarnings: Boolean;

procedure CurStepChanged(CurStep: TSetupStep);
var
  ResultCode: Integer;
  Detail: String;
begin
  if CurStep <> ssPostInstall then
    Exit;
  // esr.exe is a GUI program without a console window. SW_SHOWNORMAL so
  // its own message boxes (which name any failed steps) are shown.
  if not Exec(ExpandConstant('{tmp}\esr.exe'), '--install --quiet', ExpandConstant('{tmp}'),
              SW_SHOWNORMAL, ewWaitUntilTerminated, ResultCode) then
  begin
    InstallFailed := True;
    MsgBox('{#AppName} could not be started to install it (' + SysErrorMessage(ResultCode) + '). ' +
           'See Troubleshooting at {#TroubleshootingUrl}.', mbError, MB_OK);
    Exit;
  end;
  // Exit codes of esr.exe --install: 0 installed; 1 not installed (esr.exe
  // already showed why); 2 unsupported Windows; 3 installed, but some
  // best-effort steps failed (esr.exe already listed them).
  case ResultCode of
    0: ;
    1: InstallFailed := True;
    3: InstallWarnings := True;
  else
    begin
      InstallFailed := True;
      if ResultCode = 2 then
        Detail := 'This version of Windows is not supported. {#AppName} needs Windows 11 22H2 (build 22621.675) or later.'
      else
        Detail := 'Close {#AppName} from its Settings if it is running and run Setup again.';
      MsgBox('{#AppName} could not be installed (error code ' + IntToStr(ResultCode) + '). ' + Detail +
             ' See Troubleshooting at {#TroubleshootingUrl}.', mbError, MB_OK);
    end;
  end;
end;

procedure CurPageChanged(CurPageID: Integer);
begin
  if CurPageID <> wpFinished then
    Exit;
  if InstallFailed then
  begin
    WizardForm.FinishedHeadingLabel.Caption := '{#AppName} was not installed';
    WizardForm.FinishedLabel.Caption :=
      'Setup could not finish installing {#AppName}. ' +
      'Close {#AppName} from its Settings if it is running and run Setup again, ' +
      'or see Troubleshooting at {#TroubleshootingUrl}.';
  end
  else if InstallWarnings then
  begin
    WizardForm.FinishedHeadingLabel.Caption := '{#AppName} is installed, with warnings';
    WizardForm.FinishedLabel.Caption :=
      '{#AppName} is installed, but some steps did not finish. The message shown before this page ' +
      'listed them and what to do.' + #13#10#13#10 +
      'See Troubleshooting at {#TroubleshootingUrl}.';
  end
  else
    WizardForm.FinishedLabel.Caption :=
      '{#AppName} is installed and running in the background. Your File Explorer windows and tabs ' +
      'will be reopened after you sign in.' + #13#10#13#10 +
      'To open Settings, start {#AppName} from the Start menu again.';
end;
