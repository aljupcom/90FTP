; 90FTP - Modern Site Manager Installer Script
; Inno Setup 7 Configuration

#define MyAppName "90FTP"
#ifndef MyAppVersion
  #define MyAppVersion "0.0.0.4"
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
DefaultDirName={autopf}\{#MyAppName}
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
  SetupTypePage: TWizardPage;
  TypicalRadio: TNewRadioButton;
  CustomRadio: TNewRadioButton;
  TypicalDescLabel: TLabel;
  CustomDescLabel: TLabel;
  UpgradeInfoLabel: TLabel;
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

procedure OnRadioClick(Sender: TObject);
begin
  if IsUpgrade and TypicalRadio.Checked then
    WizardForm.NextButton.Caption := '&Upgrade'
  else
    WizardForm.NextButton.Caption := '&Next >';
end;

procedure InitializeWizard;
var
  TopPos: Integer;
begin
  WizardForm.Font.Name := 'Segoe UI';
  IsUpgrade := CheckForExistingInstallation;

  // Create Setup Type Page after wpWelcome
  SetupTypePage := CreateCustomPage(wpWelcome, 'Setup Type', 'Choose how you want to install or upgrade {#MyAppName}');

  TopPos := ScaleY(10);

  if IsUpgrade then
  begin
    UpgradeInfoLabel := TLabel.Create(SetupTypePage);
    UpgradeInfoLabel.Parent := SetupTypePage.Surface;
    UpgradeInfoLabel.Left := ScaleX(4);
    UpgradeInfoLabel.Top := TopPos;
    UpgradeInfoLabel.Width := SetupTypePage.SurfaceWidth - ScaleX(8);
    UpgradeInfoLabel.WordWrap := True;
    UpgradeInfoLabel.Font.Style := [fsBold];
    UpgradeInfoLabel.Caption := 'Setup detected an existing installation of {#MyAppName} (v' + PrevVersion + ') at:' + #13#10 + PrevAppPath;
    TopPos := TopPos + ScaleY(42);
  end;

  TypicalRadio := TNewRadioButton.Create(SetupTypePage);
  TypicalRadio.Parent := SetupTypePage.Surface;
  TypicalRadio.Left := ScaleX(4);
  TypicalRadio.Top := TopPos;
  TypicalRadio.Width := SetupTypePage.SurfaceWidth - ScaleX(8);
  TypicalRadio.Font.Style := [fsBold];
  if IsUpgrade then
    TypicalRadio.Caption := '&Full upgrade (Recommended)'
  else
    TypicalRadio.Caption := '&Typical installation (Recommended)';
  TypicalRadio.Checked := True;

  TopPos := TopPos + ScaleY(24);

  TypicalDescLabel := TLabel.Create(SetupTypePage);
  TypicalDescLabel.Parent := SetupTypePage.Surface;
  TypicalDescLabel.Left := ScaleX(24);
  TypicalDescLabel.Top := TopPos;
  TypicalDescLabel.Width := SetupTypePage.SurfaceWidth - ScaleX(32);
  TypicalDescLabel.WordWrap := True;
  if IsUpgrade then
    TypicalDescLabel.Caption := 'Upgrades existing installation directly in place to v{#MyAppVersion}. All your saved sites, sessions, vaults, and configurations will be preserved.'
  else
    TypicalDescLabel.Caption := 'Installs {#MyAppName} to the default location with standard shortcuts and configuration.';

  TopPos := TopPos + ScaleY(40);

  CustomRadio := TNewRadioButton.Create(SetupTypePage);
  CustomRadio.Parent := SetupTypePage.Surface;
  CustomRadio.Left := ScaleX(4);
  CustomRadio.Top := TopPos;
  CustomRadio.Width := SetupTypePage.SurfaceWidth - ScaleX(8);
  CustomRadio.Font.Style := [fsBold];
  if IsUpgrade then
    CustomRadio.Caption := '&Custom upgrade or new installation'
  else
    CustomRadio.Caption := '&Custom installation';

  TopPos := TopPos + ScaleY(24);

  CustomDescLabel := TLabel.Create(SetupTypePage);
  CustomDescLabel.Parent := SetupTypePage.Surface;
  CustomDescLabel.Left := ScaleX(24);
  CustomDescLabel.Top := TopPos;
  CustomDescLabel.Width := SetupTypePage.SurfaceWidth - ScaleX(32);
  CustomDescLabel.WordWrap := True;
  if IsUpgrade then
    CustomDescLabel.Caption := 'Allows choosing a different installation directory, custom components, and shortcut options.'
  else
    CustomDescLabel.Caption := 'Allows full selection of installation destination, components, and shortcut options.';
  TypicalRadio.OnClick := @OnRadioClick;
  CustomRadio.OnClick := @OnRadioClick;
end;

function ShouldSkipPage(PageID: Integer): Boolean;
begin
  Result := False;
  // If user selected Full Upgrade, skip directory and program group selection!
  if (PageID = wpSelectDir) or (PageID = wpSelectProgramGroup) then
  begin
    if IsUpgrade and TypicalRadio.Checked then
      Result := True;
  end;
end;

function NextButtonClick(CurPageID: Integer): Boolean;
begin
  Result := True;
  if CurPageID = SetupTypePage.ID then
  begin
    if IsUpgrade and TypicalRadio.Checked then
    begin
      WizardForm.DirEdit.Text := PrevAppPath;
    end;
  end;
end;

procedure CurPageChanged(CurPageID: Integer);
begin
  if CurPageID = SetupTypePage.ID then
  begin
    if IsUpgrade and TypicalRadio.Checked then
      WizardForm.NextButton.Caption := '&Upgrade'
    else
      WizardForm.NextButton.Caption := '&Next >';
  end
  else if CurPageID = wpReady then
  begin
    if IsUpgrade and TypicalRadio.Checked then
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
  if IsUpgrade and TypicalRadio.Checked then
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
