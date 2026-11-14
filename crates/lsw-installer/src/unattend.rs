//! `autounattend.xml` generation for a fully unattended Windows 11 install.
//!
//! The answer file:
//! * loads virtio storage/network drivers in Windows PE from the virtio-win
//!   ISO (whatever drive letter it gets),
//! * wipes disk 0 and creates the standard UEFI layout (EFI, MSR, Windows),
//! * selects the edition by image name (no edition prompt),
//! * creates a local administrator named after the Linux user with
//!   autologon, skipping every OOBE page (no Microsoft account, no network
//!   requirement),
//! * runs LSW's first-boot script at the first logon.
//!
//! No hardware-requirement bypasses are used: LSW provides real UEFI,
//! Secure Boot capability and TPM 2.0.

use std::fmt::Write as _;

/// virtio-win driver folders loaded in Windows PE. Storage first: setup
/// cannot see the virtio-blk disk without `viostor`.
pub const PE_DRIVERS: &[&str] = &[
    "viostor",
    "vioscsi",
    "NetKVM",
    "Balloon",
    "vioserial",
    "viorng",
];

/// Drive letters Windows PE may assign to the virtio-win ISO.
pub const PE_DRIVE_LETTERS: &[char] = &['D', 'E', 'F', 'G', 'H'];

/// Name of the serial-console line the first-boot script prints when done.
pub const INSTALL_DONE_MARKER: &str = "LSW_INSTALL_DONE";

/// Serial-console marker for a first boot whose required steps failed.
pub const INSTALL_FAILED_MARKER: &str = "LSW_INSTALL_FAILED";

/// Prefix shared by both outcome markers.
pub const INSTALL_OUTCOME_PREFIX: &str = "LSW_INSTALL_";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnattendOptions {
    /// Local account name (Windows rules: 1-20 chars, no `"/\[]:;|=,+*?<>@`).
    pub username: String,
    pub full_name: String,
    pub password: String,
    /// BCP-47 tag, e.g. `en-US`.
    pub locale: String,
    /// Windows time zone id, e.g. `UTC` or `Pacific Standard Time`.
    pub time_zone: String,
    pub computer_name: String,
    /// Image name inside `install.wim`, e.g. `Windows 11 Pro`.
    pub image_name: String,
    /// Select the image by index instead of name (single-image media such as
    /// the Evaluation ISO). The name is still used to pick a generic key.
    pub image_index: Option<u32>,
    /// Product key; `None` lets setup use the media default (evaluation media)
    /// or the generic edition key (see [`generic_key`]).
    pub product_key: Option<String>,
}

/// Microsoft's published generic KMS client setup keys, which select an
/// edition during setup without activating it. The user activates with
/// their own key afterwards.
/// Source: learn.microsoft.com/windows-server/get-started/kms-client-activation-keys
pub fn generic_key(image_name: &str) -> Option<&'static str> {
    match image_name {
        "Windows 11 Pro" => Some("W269N-WFGWX-YVC9B-4J6C3-9T44M"),
        "Windows 11 Pro for Workstations" => Some("NRG8B-VKK3Q-CXVCJ-9G2XF-6Q84J"),
        "Windows 11 Education" => Some("NW6C2-QMPVW-D7KKK-3GKT6-VCFB2"),
        "Windows 11 Enterprise" => Some("NPPR9-FWDCX-D2C8J-H872K-2YT43"),
        _ => None,
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum UnattendError {
    #[error("invalid Windows user name {0:?}")]
    Username(String),
    #[error("invalid computer name {0:?}")]
    ComputerName(String),
    #[error("password must be 1-127 characters without control characters")]
    Password,
    #[error("invalid {field}: {value:?}")]
    Field { field: &'static str, value: String },
}

impl UnattendOptions {
    pub fn validate(&self) -> Result<(), UnattendError> {
        const BAD: &[char] = &[
            '"', '/', '\\', '[', ']', ':', ';', '|', '=', ',', '+', '*', '?', '<', '>', '@', '%',
        ];
        let u = &self.username;
        if u.is_empty()
            || u.chars().count() > 20
            || u.chars().any(|c| c.is_control() || BAD.contains(&c))
            || u.trim() != u
            || u.ends_with('.')
            || ["administrator", "guest", "defaultaccount"]
                .contains(&u.to_ascii_lowercase().as_str())
        {
            return Err(UnattendError::Username(u.clone()));
        }
        let c = &self.computer_name;
        if c.is_empty()
            || c.len() > 15
            || !c.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '-')
            || c.chars().all(|ch| ch.is_ascii_digit())
        {
            return Err(UnattendError::ComputerName(c.clone()));
        }
        if self.password.is_empty()
            || self.password.chars().count() > 127
            || self.password.chars().any(char::is_control)
        {
            return Err(UnattendError::Password);
        }
        let simple = |field: &'static str, v: &str, ok: fn(char) -> bool| {
            if v.is_empty() || v.len() > 64 || !v.chars().all(ok) {
                Err(UnattendError::Field {
                    field,
                    value: v.to_owned(),
                })
            } else {
                Ok(())
            }
        };
        simple("locale", &self.locale, |c| {
            c.is_ascii_alphanumeric() || c == '-'
        })?;
        simple("time zone", &self.time_zone, |c| {
            c.is_ascii_alphanumeric() || " ()+-.".contains(c)
        })?;
        simple("image name", &self.image_name, |c| {
            c.is_ascii_alphanumeric() || " -".contains(c)
        })?;
        if let Some(k) = &self.product_key {
            simple("product key", k, |c| c.is_ascii_alphanumeric() || c == '-')?;
        }
        if self.full_name.chars().count() > 64 || self.full_name.chars().any(char::is_control) {
            return Err(UnattendError::Field {
                field: "full name",
                value: self.full_name.clone(),
            });
        }
        Ok(())
    }
}

/// XML-escape text content and attribute values.
pub fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

const COMPONENT_ATTRS: &str = r#"processorArchitecture="amd64" publicKeyToken="31bf3856ad364e35" language="neutral" versionScope="nonSxS""#;

/// Render the answer file.
pub fn render(o: &UnattendOptions) -> Result<String, UnattendError> {
    o.validate()?;
    let e = xml_escape;
    let locale = e(&o.locale);
    let user = e(&o.username);
    let full = e(if o.full_name.is_empty() {
        &o.username
    } else {
        &o.full_name
    });
    let pw = e(&o.password);
    let key = o
        .product_key
        .clone()
        .or_else(|| generic_key(&o.image_name).map(str::to_owned));

    let mut driver_paths = String::new();
    let mut n = 1;
    for letter in PE_DRIVE_LETTERS {
        for drv in PE_DRIVERS {
            let _ = write!(
                driver_paths,
                "\n        <PathAndCredentials wcm:action=\"add\" wcm:keyValue=\"{n}\"><Path>{letter}:\\{drv}\\w11\\amd64</Path></PathAndCredentials>"
            );
            n += 1;
        }
    }

    let product_key = match key {
        Some(k) => format!(
            "\n        <ProductKey><Key>{}</Key><WillShowUI>OnError</WillShowUI></ProductKey>",
            e(&k)
        ),
        None => String::new(),
    };

    let first_logon = r#"cmd /c "for %d in (D E F G H I J K) do if exist %d:\lsw\firstboot.ps1 powershell -NoProfile -ExecutionPolicy Bypass -File %d:\lsw\firstboot.ps1 -Media %d:""#;

    Ok(format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<!-- Generated by LSW. Removed from the VM after installation. -->
<unattend xmlns="urn:schemas-microsoft-com:unattend" xmlns:wcm="http://schemas.microsoft.com/WMIConfig/2002/State">
  <settings pass="windowsPE">
    <component name="Microsoft-Windows-International-Core-WinPE" {COMPONENT_ATTRS}>
      <SetupUILanguage><UILanguage>{locale}</UILanguage></SetupUILanguage>
      <InputLocale>{locale}</InputLocale>
      <SystemLocale>{locale}</SystemLocale>
      <UILanguage>{locale}</UILanguage>
      <UserLocale>{locale}</UserLocale>
    </component>
    <component name="Microsoft-Windows-PnpCustomizationsWinPE" {COMPONENT_ATTRS}>
      <DriverPaths>{driver_paths}
      </DriverPaths>
    </component>
    <component name="Microsoft-Windows-Setup" {COMPONENT_ATTRS}>
      <DynamicUpdate><Enable>false</Enable><WillShowUI>Never</WillShowUI></DynamicUpdate>
      <DiskConfiguration>
        <WillShowUI>OnError</WillShowUI>
        <Disk wcm:action="add">
          <DiskID>0</DiskID>
          <WillWipeDisk>true</WillWipeDisk>
          <CreatePartitions>
            <CreatePartition wcm:action="add"><Order>1</Order><Type>EFI</Type><Size>260</Size></CreatePartition>
            <CreatePartition wcm:action="add"><Order>2</Order><Type>MSR</Type><Size>16</Size></CreatePartition>
            <CreatePartition wcm:action="add"><Order>3</Order><Type>Primary</Type><Extend>true</Extend></CreatePartition>
          </CreatePartitions>
          <ModifyPartitions>
            <ModifyPartition wcm:action="add"><Order>1</Order><PartitionID>1</PartitionID><Label>System</Label><Format>FAT32</Format></ModifyPartition>
            <ModifyPartition wcm:action="add"><Order>2</Order><PartitionID>2</PartitionID></ModifyPartition>
            <ModifyPartition wcm:action="add"><Order>3</Order><PartitionID>3</PartitionID><Label>Windows</Label><Letter>C</Letter><Format>NTFS</Format></ModifyPartition>
          </ModifyPartitions>
        </Disk>
      </DiskConfiguration>
      <ImageInstall>
        <OSImage>
          <InstallFrom><MetaData wcm:action="add"><Key>{image_key}</Key><Value>{image}</Value></MetaData></InstallFrom>
          <InstallTo><DiskID>0</DiskID><PartitionID>3</PartitionID></InstallTo>
          <WillShowUI>OnError</WillShowUI>
        </OSImage>
      </ImageInstall>
      <UserData>
        <AcceptEula>true</AcceptEula>{product_key}
      </UserData>
    </component>
  </settings>
  <settings pass="specialize">
    <component name="Microsoft-Windows-Shell-Setup" {COMPONENT_ATTRS}>
      <ComputerName>{computer}</ComputerName>
      <TimeZone>{tz}</TimeZone>
    </component>
  </settings>
  <settings pass="oobeSystem">
    <component name="Microsoft-Windows-International-Core" {COMPONENT_ATTRS}>
      <InputLocale>{locale}</InputLocale>
      <SystemLocale>{locale}</SystemLocale>
      <UILanguage>{locale}</UILanguage>
      <UserLocale>{locale}</UserLocale>
    </component>
    <component name="Microsoft-Windows-Shell-Setup" {COMPONENT_ATTRS}>
      <OOBE>
        <HideEULAPage>true</HideEULAPage>
        <HideOEMRegistrationScreen>true</HideOEMRegistrationScreen>
        <HideOnlineAccountScreens>true</HideOnlineAccountScreens>
        <HideLocalAccountScreen>true</HideLocalAccountScreen>
        <HideWirelessSetupInOOBE>true</HideWirelessSetupInOOBE>
        <ProtectYourPC>3</ProtectYourPC>
      </OOBE>
      <UserAccounts>
        <LocalAccounts>
          <LocalAccount wcm:action="add">
            <Name>{user}</Name>
            <DisplayName>{full}</DisplayName>
            <Group>Administrators</Group>
            <Password><Value>{pw}</Value><PlainText>true</PlainText></Password>
          </LocalAccount>
        </LocalAccounts>
      </UserAccounts>
      <AutoLogon>
        <Enabled>true</Enabled>
        <Username>{user}</Username>
        <Password><Value>{pw}</Value><PlainText>true</PlainText></Password>
        <LogonCount>9999999</LogonCount>
      </AutoLogon>
      <FirstLogonCommands>
        <SynchronousCommand wcm:action="add">
          <Order>1</Order>
          <CommandLine>{first_logon}</CommandLine>
          <Description>LSW first boot</Description>
          <RequiresUserInput>false</RequiresUserInput>
        </SynchronousCommand>
      </FirstLogonCommands>
    </component>
  </settings>
</unattend>
"#,
        image_key = if o.image_index.is_some() {
            "/IMAGE/INDEX"
        } else {
            "/IMAGE/NAME"
        },
        image = o
            .image_index
            .map_or_else(|| e(&o.image_name), |i| i.to_string()),
        computer = e(&o.computer_name),
        tz = e(&o.time_zone),
        first_logon = e(first_logon),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts() -> UnattendOptions {
        UnattendOptions {
            username: "alex".into(),
            full_name: "Alex <A&B>".into(),
            password: "p\"w<&>".into(),
            locale: "en-US".into(),
            time_zone: "UTC".into(),
            computer_name: "LSW-WIN".into(),
            image_name: "Windows 11 Pro".into(),
            image_index: None,
            product_key: None,
        }
    }

    #[test]
    fn renders_escaped_wellformed_xml() {
        let xml = render(&opts()).unwrap();
        assert!(xml.contains("<Name>alex</Name>"));
        assert!(xml.contains("<DisplayName>Alex &lt;A&amp;B&gt;</DisplayName>"));
        assert!(xml.contains("<Value>p&quot;w&lt;&amp;&gt;</Value>"));
        assert!(xml.contains("<Value>Windows 11 Pro</Value>"));
        // Generic edition key, not an activation key.
        assert!(xml.contains("W269N-WFGWX-YVC9B-4J6C3-9T44M"));
        assert!(xml.contains(r"D:\viostor\w11\amd64"));
        assert!(xml.contains(r"H:\NetKVM\w11\amd64"));
        assert!(xml.contains("HideOnlineAccountScreens>true"));
        assert!(!xml.contains("LabConfig"), "no requirement bypasses");
        // Every opened element is closed: a cheap well-formedness check.
        let opens = xml.matches("<settings ").count();
        assert_eq!(opens, xml.matches("</settings>").count());
        assert_eq!(
            xml.matches("<component ").count(),
            xml.matches("</component>").count()
        );
        // Unique wcm:keyValue per driver path.
        assert_eq!(
            xml.matches("wcm:keyValue=").count(),
            PE_DRIVERS.len() * PE_DRIVE_LETTERS.len()
        );
    }

    #[test]
    fn evaluation_media_gets_no_key_when_unknown_image() {
        let mut o = opts();
        o.image_name = "Windows 11 Enterprise Evaluation".into();
        o.image_index = Some(1);
        let xml = render(&o).unwrap();
        assert!(xml.contains("<Key>/IMAGE/INDEX</Key><Value>1</Value>"));
        assert!(!xml.contains("<ProductKey>"));
    }

    #[test]
    fn explicit_key_wins() {
        let mut o = opts();
        o.product_key = Some("AAAAA-BBBBB-CCCCC-DDDDD-EEEEE".into());
        let xml = render(&o).unwrap();
        assert!(xml.contains("<Key>AAAAA-BBBBB-CCCCC-DDDDD-EEEEE</Key>"));
        assert!(!xml.contains("W269N"));
    }

    #[test]
    fn rejects_bad_input() {
        for bad in [
            "",
            "Administrator",
            "a/b",
            "x".repeat(21).as_str(),
            "trailing.",
        ] {
            let mut o = opts();
            o.username = bad.into();
            assert!(
                matches!(render(&o), Err(UnattendError::Username(_))),
                "{bad:?}"
            );
        }
        let mut o = opts();
        o.computer_name = "1234".into();
        assert!(render(&o).is_err());
        let mut o = opts();
        o.password = "a\nb".into();
        assert_eq!(render(&o), Err(UnattendError::Password));
        let mut o = opts();
        o.image_name = "Pro</Value><x>".into();
        assert!(render(&o).is_err());
    }
}
