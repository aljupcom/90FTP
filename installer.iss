; 90FTP - Modern Site Manager Installer Script
; Inno Setup 7 Configuration

#define MyAppName "90FTP"
#ifndef MyAppVersion
  #define MyAppVersion "0.0.0.67"
#endif
#define MyAppPublisher "aljailane"
#define MyAppURL "https://github.com/aljupcom/90FTP"
#define MyAppExeName "90FTP.exe"

[Setup]
AppId={{E6F7A9C0-3D5B-4C91-8D92-7E1A2B3C4D5E}
AppName={#MyAppName}
AppVersion={#MyAppVersion}
AppVerName={#MyAppName} v{#MyAppVersion}
VersionInfoDescription={#MyAppName} Setup
VersionInfoProductName={#MyAppName}
VersionInfoCompany={#MyAppPublisher}
VersionInfoCopyright=Copyright (C) 2026 {#MyAppPublisher}
AppPublisher={#MyAppPublisher}
AppPublisherURL={#MyAppURL}
AppSupportURL={#MyAppURL}
AppUpdatesURL={#MyAppURL}
DefaultDirName={code:GetAppDir}
DefaultGroupName={#MyAppName}
AllowNoIcons=yes
OutputDir=dist
OutputBaseFilename={#MyAppName}-Setup-v{#MyAppVersion}
SetupIconFile=assets\app_icon.ico
UninstallDisplayIcon={app}\{#MyAppExeName}
UninstallDisplayName={#MyAppName} - Modern Site Manager
Compression=lzma2/ultra64
SolidCompression=yes
WizardStyle=modern
WizardImageFile=assets\wizard_large.bmp
WizardSmallImageFile=assets\wizard_small.bmp
PrivilegesRequiredOverridesAllowed=dialog
ArchitecturesInstallIn64BitMode=x64compatible
CloseApplications=yes
RestartApplications=no
DisableWelcomePage=no

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
Source: "target\release\ninety_ftp.exe"; DestDir: "{app}"; DestName: "{#MyAppExeName}"; Flags: ignoreversion
Source: "assets\app_icon.ico"; DestDir: "{app}"; Flags: ignoreversion
Source: "assets\app_icon.png"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{group}\{#MyAppName}"; Filename: "{app}\{#MyAppExeName}"; IconFilename: "{app}\app_icon.ico"
Name: "{group}\{cm:UninstallProgram,{#MyAppName}}"; Filename: "{uninstallexe}"
Name: "{autodesktop}\{#MyAppName}"; Filename: "{app}\{#MyAppExeName}"; IconFilename: "{app}\app_icon.ico"; Tasks: desktopicon

[Run]
Filename: "{app}\{#MyAppExeName}"; Description: "{cm:LaunchProgram,{#StringChange(MyAppName, '&', '&&')}}"; Flags: nowait postinstall skipifsilent

[Code]
var
  SetupTypePage: TInputOptionWizardPage;
  IsUpgrade: Boolean;
  PrevAppPath: String;
  PrevVersion: String;

const
  AppUninstallGuid = '{E6F7A9C0-3D5B-4C91-8D92-7E1A2B3C4D5E}_is1';

function CheckForExistingInstallation: Boolean;
var
  RegKey: String;
  PathVal: String;
  VerVal: String;
begin
  Result := False;
  RegKey := 'Software\Microsoft\Windows\CurrentVersion\Uninstall\' + AppUninstallGuid;
  
  // Check HKLM 64-bit and 32-bit, and HKCU
  if RegQueryStringValue(HKLM64, RegKey, 'Inno Setup: App Path', PathVal) or
     RegQueryStringValue(HKLM32, RegKey, 'Inno Setup: App Path', PathVal) or
     RegQueryStringValue(HKCU64, RegKey, 'Inno Setup: App Path', PathVal) or
     RegQueryStringValue(HKCU32, RegKey, 'Inno Setup: App Path', PathVal) or
     RegQueryStringValue(HKLM, RegKey, 'Inno Setup: App Path', PathVal) or
     RegQueryStringValue(HKCU, RegKey, 'Inno Setup: App Path', PathVal) then
  begin
    if DirExists(PathVal) then
    begin
      PrevAppPath := PathVal;
      Result := True;
      if not (RegQueryStringValue(HKLM64, RegKey, 'DisplayVersion', VerVal) or
              RegQueryStringValue(HKLM32, RegKey, 'DisplayVersion', VerVal) or
              RegQueryStringValue(HKCU, RegKey, 'DisplayVersion', VerVal) or
              RegQueryStringValue(HKLM, RegKey, 'DisplayVersion', VerVal)) then
      begin
        VerVal := 'Unknown';
      end;
      PrevVersion := VerVal;
    end;
  end;
end;

function GetAppDir(Param: String): String;
begin
  if CheckForExistingInstallation and (PrevAppPath <> '') then
    Result := PrevAppPath
  else
    Result := ExpandConstant('{autopf}\{#MyAppName}');
end;

procedure InitializeWizard;
var
  SubCaptionText: String;
begin
  WizardForm.Font.Name := 'Segoe UI';
  IsUpgrade := CheckForExistingInstallation;

  if IsUpgrade then
    SubCaptionText := 'Existing installation detected: 90FTP v' + PrevVersion + ' at ' + PrevAppPath + #13#10 + 'Please select how you want to proceed:'
  else
    SubCaptionText := 'Please select the setup type that best suits your needs:';

  SetupTypePage := CreateInputOptionPage(
    wpWelcome,
    'Setup Type',
    'Choose how you want to install or upgrade {#MyAppName}',
    SubCaptionText,
    True,
    False
  );

  if IsUpgrade then
  begin
    SetupTypePage.Add('&Full upgrade (Recommended) - Upgrades in place to v{#MyAppVersion} preserving all settings');
    SetupTypePage.Add('&Custom installation - Choose destination folder, components, and shortcuts');
  end
  else
  begin
    SetupTypePage.Add('&Typical installation (Recommended) - Installs 90FTP to default location');
    SetupTypePage.Add('&Custom installation - Choose destination folder, components, and shortcuts');
  end;

  SetupTypePage.SelectedValueIndex := 0;
end;

function ShouldSkipPage(PageID: Integer): Boolean;
begin
  Result := False;
  // If user selected Full Upgrade (Option index 0), skip directory and program group selection!
  if (PageID = wpSelectDir) or (PageID = wpSelectProgramGroup) then
  begin
    if IsUpgrade and (SetupTypePage.SelectedValueIndex = 0) then
      Result := True;
  end;
end;

function NextButtonClick(CurPageID: Integer): Boolean;
begin
  Result := True;
  if CurPageID = SetupTypePage.ID then
  begin
    if IsUpgrade and (SetupTypePage.SelectedValueIndex = 0) then
    begin
      WizardForm.DirEdit.Text := PrevAppPath;
    end;
  end;
end;

procedure CurPageChanged(CurPageID: Integer);
begin
  if CurPageID = SetupTypePage.ID then
  begin
    if IsUpgrade and (SetupTypePage.SelectedValueIndex = 0) then
      WizardForm.NextButton.Caption := '&Upgrade'
    else
      WizardForm.NextButton.Caption := '&Next >';
  end
  else if CurPageID = wpReady then
  begin
    if IsUpgrade and (SetupTypePage.SelectedValueIndex = 0) then
      WizardForm.NextButton.Caption := '&Upgrade'
    else
      WizardForm.NextButton.Caption := '&Install';
  end;
end;

function UpdateReadyMemo(Space, NewLine, MemoUserInfoInfo, MemoDirInfo, MemoTypeInfo,
  MemoComponentsInfo, MemoGroupInfo, MemoTasksInfo: String): String;
var
  S: String;
begin
  if IsUpgrade and (SetupTypePage.SelectedValueIndex = 0) then
  begin
    S := 'Upgrade Mode: Full upgrade to v' + '{#MyAppVersion}' + NewLine;
    S := S + 'Previous Version: v' + PrevVersion + NewLine;
    S := S + 'Target Directory:' + NewLine + Space + PrevAppPath + NewLine + NewLine;
    S := S + 'Status: Preserving all user settings, saved sites, and credentials.' + NewLine;
    Result := S;
  end
  else
  begin
    S := '';
    if MemoDirInfo <> '' then S := S + MemoDirInfo + NewLine + NewLine;
    if MemoGroupInfo <> '' then S := S + MemoGroupInfo + NewLine + NewLine;
    if MemoTasksInfo <> '' then S := S + MemoTasksInfo + NewLine + NewLine;
    Result := S;
  end;
end;

procedure CurStepChanged(CurStep: TSetupStep);
var
  ResultCode: Integer;
begin
  if CurStep = ssInstall then
  begin
    Exec('taskkill.exe', '/F /IM 90FTP.exe /T', '', SW_HIDE, ewWaitUntilTerminated, ResultCode);
    Exec('taskkill.exe', '/F /IM ninety_ftp.exe /T', '', SW_HIDE, ewWaitUntilTerminated, ResultCode);
    Sleep(600);
  end;
end;
