//! Which entitlements this app was actually signed with (CLAUDE.md section 9.2).
//!
//! SideStore re-signs the app on the phone with the user's free certificate and
//! keeps only the entitlements that certificate's provisioning profile allows. Two
//! sources are read:
//!
//! - the code signature embedded in the executable (what the kernel enforces),
//! - `embedded.mobileprovision` in the bundle (what the profile permits).
//!
//! An entitlement counts as granted only when the signature claims it and, if a
//! profile is present, the profile allows it.

use std::path::Path;

use anyhow::{Context as _, Result, bail};

pub const INCREASED_MEMORY_LIMIT: &str = "com.apple.developer.kernel.increased-memory-limit";
pub const GET_TASK_ALLOW: &str = "get-task-allow";

const LC_CODE_SIGNATURE: u32 = 0x1d;
const MH_MAGIC_64: u32 = 0xfeed_facf;
const CSMAGIC_EMBEDDED_SIGNATURE: u32 = 0xfade_0cc0;
const CSMAGIC_EMBEDDED_ENTITLEMENTS: u32 = 0xfade_7171;
const CSSLOT_ENTITLEMENTS: u32 = 5;

/// Extracts the XML entitlements plist from a Mach-O executable's signature.
/// Returns `Ok(None)` for an unsigned binary or a signature without entitlements.
pub fn signature_entitlements(executable: &[u8]) -> Result<Option<String>> {
    let read_u32_le = |offset: usize| -> Result<u32> {
        let bytes = executable
            .get(offset..offset + 4)
            .context("truncated Mach-O header")?;
        Ok(u32::from_le_bytes(bytes.try_into()?))
    };
    if read_u32_le(0)? != MH_MAGIC_64 {
        bail!("not a 64-bit little-endian Mach-O file");
    }
    let command_count = read_u32_le(16)? as usize;
    let mut offset = 32; // sizeof(mach_header_64)
    let mut signature = None;
    for _ in 0..command_count {
        let command = read_u32_le(offset)?;
        let size = read_u32_le(offset + 4)? as usize;
        if command == LC_CODE_SIGNATURE {
            let data_offset = read_u32_le(offset + 8)? as usize;
            let data_size = read_u32_le(offset + 12)? as usize;
            signature = Some(
                executable
                    .get(data_offset..data_offset + data_size)
                    .context("code signature outside the file")?,
            );
            break;
        }
        if size == 0 {
            bail!("malformed load command");
        }
        offset += size;
    }
    let Some(signature) = signature else {
        return Ok(None);
    };

    // The signature superblob and its index are big-endian.
    let read_u32_be = |bytes: &[u8], offset: usize| -> Result<u32> {
        let bytes = bytes
            .get(offset..offset + 4)
            .context("truncated code signature")?;
        Ok(u32::from_be_bytes(bytes.try_into()?))
    };
    if read_u32_be(signature, 0)? != CSMAGIC_EMBEDDED_SIGNATURE {
        bail!("unexpected code signature magic");
    }
    let blob_count = read_u32_be(signature, 8)? as usize;
    for index in 0..blob_count {
        let entry = 12 + index * 8;
        let slot = read_u32_be(signature, entry)?;
        let blob_offset = read_u32_be(signature, entry + 4)? as usize;
        if slot != CSSLOT_ENTITLEMENTS {
            continue;
        }
        if read_u32_be(signature, blob_offset)? != CSMAGIC_EMBEDDED_ENTITLEMENTS {
            bail!("unexpected entitlements blob magic");
        }
        let length = read_u32_be(signature, blob_offset + 4)? as usize;
        let xml = signature
            .get(blob_offset + 8..blob_offset + length)
            .context("truncated entitlements blob")?;
        return Ok(Some(String::from_utf8_lossy(xml).into_owned()));
    }
    Ok(None)
}

/// Extracts the XML plist from a provisioning profile (a CMS envelope around a
/// plain XML plist, which can be located without parsing the envelope).
pub fn provisioning_profile_plist(profile: &[u8]) -> Option<String> {
    let start = find(profile, b"<?xml")?;
    let end = find(&profile[start..], b"</plist>")? + start + b"</plist>".len();
    Some(String::from_utf8_lossy(&profile[start..end]).into_owned())
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Whether a plist sets `key` to `<true/>`. Entitlement plists are flat enough
/// that the value immediately follows the key.
pub fn plist_bool(plist: &str, key: &str) -> Option<bool> {
    let marker = format!("<key>{key}</key>");
    let after = &plist[plist.find(&marker)? + marker.len()..];
    let value = after.trim_start();
    if value.starts_with("<true/>") {
        Some(true)
    } else if value.starts_with("<false/>") {
        Some(false)
    } else {
        None
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Grant {
    Granted,
    NotGranted,
    /// No signature could be read (for example, an unsigned Simulator build).
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntitlementReport {
    pub increased_memory_limit: Grant,
    pub get_task_allow: Grant,
    pub has_provisioning_profile: bool,
}

fn grant(signature: Option<&str>, profile: Option<&str>, key: &str) -> Grant {
    let Some(signature) = signature else {
        return Grant::Unknown;
    };
    let signed = plist_bool(signature, key).unwrap_or(false);
    // The profile nests entitlements under <key>Entitlements</key>; a flat search
    // for the key still finds it there.
    let allowed = profile.is_none_or(|profile| plist_bool(profile, key).unwrap_or(false));
    if signed && allowed {
        Grant::Granted
    } else {
        Grant::NotGranted
    }
}

pub fn report_from(signature: Option<&str>, profile: Option<&str>) -> EntitlementReport {
    EntitlementReport {
        increased_memory_limit: grant(signature, profile, INCREASED_MEMORY_LIMIT),
        get_task_allow: grant(signature, profile, GET_TASK_ALLOW),
        has_provisioning_profile: profile.is_some(),
    }
}

/// Reads this app's executable and bundled provisioning profile.
pub fn current_report() -> EntitlementReport {
    let signature = std::env::current_exe()
        .context("locating the executable")
        .and_then(|path| std::fs::read(&path).context("reading the executable"))
        .and_then(|bytes| signature_entitlements(&bytes));
    let signature = match signature {
        Ok(signature) => signature,
        Err(error) => {
            log::warn!("could not read the code signature: {error:#}");
            None
        }
    };
    let profile = std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(|bundle| bundle.join("embedded.mobileprovision")))
        .and_then(|path| read_profile(&path));
    report_from(signature.as_deref(), profile.as_deref())
}

fn read_profile(path: &Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    provisioning_profile_plist(&bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ENTITLEMENTS: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict>
	<key>com.apple.developer.kernel.increased-memory-limit</key>
	<true/>
	<key>get-task-allow</key>
	<true/>
</dict></plist>"#;

    /// Builds a minimal Mach-O with an LC_CODE_SIGNATURE pointing at a superblob
    /// that holds `entitlements`.
    fn fake_executable(entitlements: Option<&str>) -> Vec<u8> {
        let mut blob = Vec::new();
        if let Some(entitlements) = entitlements {
            blob.extend_from_slice(&CSMAGIC_EMBEDDED_ENTITLEMENTS.to_be_bytes());
            blob.extend_from_slice(&((entitlements.len() + 8) as u32).to_be_bytes());
            blob.extend_from_slice(entitlements.as_bytes());
        }
        let mut superblob = Vec::new();
        let count: u32 = if entitlements.is_some() { 1 } else { 0 };
        let header_length = 12 + 8 * count as usize;
        superblob.extend_from_slice(&CSMAGIC_EMBEDDED_SIGNATURE.to_be_bytes());
        superblob.extend_from_slice(&((header_length + blob.len()) as u32).to_be_bytes());
        superblob.extend_from_slice(&count.to_be_bytes());
        if entitlements.is_some() {
            superblob.extend_from_slice(&CSSLOT_ENTITLEMENTS.to_be_bytes());
            superblob.extend_from_slice(&(header_length as u32).to_be_bytes());
        }
        superblob.extend_from_slice(&blob);

        let mut file = Vec::new();
        file.extend_from_slice(&MH_MAGIC_64.to_le_bytes());
        file.extend_from_slice(&[0; 12]); // cputype, cpusubtype, filetype
        file.extend_from_slice(&1u32.to_le_bytes()); // ncmds
        file.extend_from_slice(&16u32.to_le_bytes()); // sizeofcmds
        file.extend_from_slice(&[0; 8]); // flags, reserved
        let signature_offset = 32 + 16;
        file.extend_from_slice(&LC_CODE_SIGNATURE.to_le_bytes());
        file.extend_from_slice(&16u32.to_le_bytes());
        file.extend_from_slice(&(signature_offset as u32).to_le_bytes());
        file.extend_from_slice(&(superblob.len() as u32).to_le_bytes());
        file.extend_from_slice(&superblob);
        file
    }

    #[test]
    fn reads_entitlements_from_a_signature() {
        let executable = fake_executable(Some(ENTITLEMENTS));
        let xml = signature_entitlements(&executable).unwrap().unwrap();
        assert_eq!(plist_bool(&xml, INCREASED_MEMORY_LIMIT), Some(true));
        assert_eq!(plist_bool(&xml, "missing"), None);
    }

    #[test]
    fn signature_without_entitlements() {
        let executable = fake_executable(None);
        assert_eq!(signature_entitlements(&executable).unwrap(), None);
        assert!(signature_entitlements(b"not a mach-o file").is_err());
    }

    #[test]
    fn granted_needs_signature_and_profile() {
        let profile_allowing = "<plist><dict><key>Entitlements</key><dict>\
            <key>com.apple.developer.kernel.increased-memory-limit</key><true/>\
            </dict></dict></plist>";
        let profile_denying = "<plist><dict><key>Entitlements</key><dict>\
            <key>get-task-allow</key><true/></dict></dict></plist>";

        let report = report_from(Some(ENTITLEMENTS), Some(profile_allowing));
        assert_eq!(report.increased_memory_limit, Grant::Granted);
        assert_eq!(report.get_task_allow, Grant::NotGranted);

        let report = report_from(Some(ENTITLEMENTS), Some(profile_denying));
        assert_eq!(report.increased_memory_limit, Grant::NotGranted);
        assert_eq!(report.get_task_allow, Grant::Granted);

        let report = report_from(Some(ENTITLEMENTS), None);
        assert_eq!(report.increased_memory_limit, Grant::Granted);
        assert!(!report.has_provisioning_profile);

        let report = report_from(None, None);
        assert_eq!(report.increased_memory_limit, Grant::Unknown);
    }

    #[test]
    fn extracts_profile_plist_from_cms_envelope() {
        let mut profile = vec![0x30, 0x82, 0x01, 0x02];
        profile.extend_from_slice(b"<?xml version=\"1.0\"?><plist><dict/></plist>");
        profile.extend_from_slice(&[0xa0, 0x00]);
        assert_eq!(
            provisioning_profile_plist(&profile).as_deref(),
            Some("<?xml version=\"1.0\"?><plist><dict/></plist>")
        );
    }

    #[cfg(any(target_os = "ios", target_os = "macos"))]
    #[test]
    fn reads_the_test_binary() {
        // Test binaries are ad-hoc signed by the linker on Apple Silicon.
        let path = std::env::current_exe().unwrap();
        let bytes = std::fs::read(path).unwrap();
        signature_entitlements(&bytes).unwrap();
    }
}
