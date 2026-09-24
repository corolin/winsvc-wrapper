//! SDDL security descriptors applied to the service object.

use windows::Win32::Foundation::{HLOCAL, LocalFree};
use windows::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{
    DACL_SECURITY_INFORMATION, GROUP_SECURITY_INFORMATION, LookupAccountNameW,
    OBJECT_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID,
    SID_NAME_USE,
};
use windows::Win32::System::Services::{
    CloseServiceHandle, OpenSCManagerW, OpenServiceW, SC_HANDLE, SC_MANAGER_ALL_ACCESS,
    SERVICE_ALL_ACCESS, SetServiceObjectSecurity,
};
use windows::core::{PCWSTR, PWSTR};

/// DACL template for [service] allow_start_stop: admins (BA) and SYSTEM (SY)
/// keep full control, interactive users (IU) and service logons (SU) keep the
/// SCM-default read set (so `rsw status` keeps working unelevated), and each
/// delegated SID gets start/stop/query-status (RP/WP/LC). RP=SERVICE_START,
/// WP=SERVICE_STOP, LC=SERVICE_QUERY_STATUS in SDDL service-object mapping.
pub const DELEGATED_BASE_SDDL: &str =
    "D:P(A;;GA;;;BA)(A;;GA;;;SY)(A;;CCLCSWLOCRRC;;;IU)(A;;CCLCSWLOCRRC;;;SU)";

/// Builds the delegated-control DACL from already-stringified SIDs.
pub fn build_delegated_sddl(sids: &[String]) -> String {
    let mut sddl = String::from(DELEGATED_BASE_SDDL);
    for sid in sids {
        sddl.push_str("(A;;RPWPLC;;;");
        sddl.push_str(sid);
        sddl.push(')');
    }
    sddl
}

/// Resolves an account name (`BUILTIN\Users`, `DOMAIN\ops`, `.\svc`) to its
/// string SID for embedding in SDDL. Needs no elevation; resolves local and
/// (when domain-joined) domain accounts.
pub fn account_to_sid_string(account: &str) -> anyhow::Result<String> {
    let account_w: Vec<u16> = account.encode_utf16().chain([0]).collect();
    let mut sid_size = 0u32;
    let mut domain_size = 0u32;
    let mut use_kind = SID_NAME_USE::default();
    unsafe {
        // Size probe: fails with ERROR_INSUFFICIENT_BUFFER and fills the
        // sizes. A failed probe with sid_size == 0 means the account itself
        // was not found (or the lookup otherwise failed) — report that.
        if let Err(e) = LookupAccountNameW(
            PCWSTR::null(),
            PCWSTR(account_w.as_ptr()),
            None,
            &mut sid_size,
            None,
            &mut domain_size,
            &mut use_kind,
        ) && sid_size == 0
        {
            anyhow::bail!("cannot resolve account `{account}`: {e}");
        }

        let mut sid_buf = vec![0u8; sid_size as usize];
        let mut domain_buf = vec![0u16; domain_size as usize];
        LookupAccountNameW(
            PCWSTR::null(),
            PCWSTR(account_w.as_ptr()),
            Some(PSID(sid_buf.as_mut_ptr().cast())),
            &mut sid_size,
            Some(PWSTR(domain_buf.as_mut_ptr())),
            &mut domain_size,
            &mut use_kind,
        )
        .map_err(|e| anyhow::anyhow!("cannot resolve account `{account}`: {e}"))?;

        let mut sid_str = PWSTR::null();
        ConvertSidToStringSidW(PSID(sid_buf.as_mut_ptr().cast()), &mut sid_str)
            .map_err(|e| anyhow::anyhow!("cannot stringify the SID of `{account}`: {e}"))?;
        let out = sid_str
            .to_string()
            .map_err(|e| anyhow::anyhow!("invalid SID string for `{account}`: {e}"));
        let _ = LocalFree(Some(HLOCAL(sid_str.as_ptr().cast())));
        out
    }
}

/// Derives the SECURITY_INFORMATION flags from the SDDL string grammar.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
struct SddlInfoFlags {
    owner: bool,
    group: bool,
    dacl: bool,
}

impl SddlInfoFlags {
    fn is_empty(&self) -> bool {
        !(self.owner || self.group || self.dacl)
    }
}

/// The O:/G:/D: section tags live in the SDDL header (before the first
/// '('), so scanning only the header cannot be fooled by ACE contents —
/// a conditional-ACE string could otherwise contain an "O:"/"D:"
/// substring and request a component the descriptor does not carry.
fn info_flags_for_sddl(sddl: &str) -> SddlInfoFlags {
    let header = sddl.split('(').next().unwrap_or("");
    SddlInfoFlags {
        owner: header.contains("O:"),
        group: header.contains("G:"),
        dacl: header.contains("D:"),
    }
}

/// Applies an SDDL string as the service's owner/group/DACL.
///
/// `windows-service` does not expose object security, so this opens the
/// service handle directly and calls SetServiceObjectSecurity.
pub fn apply_security_descriptor(service_id: &str, sddl: &str) -> anyhow::Result<()> {
    let sddl_w: Vec<u16> = sddl.encode_utf16().chain([0]).collect();
    unsafe {
        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            PCWSTR(sddl_w.as_ptr()),
            SDDL_REVISION_1,
            &mut descriptor,
            None,
        )
        .map_err(|e| anyhow::anyhow!("invalid security_descriptor `{sddl}`: {e}"))?;

        // SECURITY_INFORMATION must only name components the descriptor
        // actually carries: requesting OWNER/GROUP on a DACL-only descriptor
        // (e.g. the delegated allow_start_stop DACL, "D:P(...)") fails with
        // ERROR_INVALID_PARAMETER (87). S: (SACL) is deliberately not
        // requested — setting it needs SE_SECURITY_NAME and no config path
        // produces one today.
        let flags = info_flags_for_sddl(sddl);
        if flags.is_empty() {
            anyhow::bail!("security descriptor carries no owner/group/DACL section");
        }
        let mut info = OBJECT_SECURITY_INFORMATION(0);
        if flags.owner {
            info |= OWNER_SECURITY_INFORMATION;
        }
        if flags.group {
            info |= GROUP_SECURITY_INFORMATION;
        }
        if flags.dacl {
            info |= DACL_SECURITY_INFORMATION;
        }

        let service = open_service(service_id)?;
        let result = SetServiceObjectSecurity(service, info, descriptor)
            .map_err(|e| anyhow::anyhow!("SetServiceObjectSecurity failed: {e}"));
        let _ = CloseServiceHandle(service);
        // The self-relative descriptor is heap-allocated by the API; free it
        // on every path (errors too — it was allocated before the call).
        let _ = windows::Win32::Foundation::LocalFree(Some(windows::Win32::Foundation::HLOCAL(
            descriptor.0 as _,
        )));
        result
    }
}

/// Opens a raw SCM service handle with full access (for APIs the
/// windows-service crate does not wrap).
pub fn open_service(service_id: &str) -> anyhow::Result<SC_HANDLE> {
    let service_w: Vec<u16> = service_id.encode_utf16().chain([0]).collect();
    unsafe {
        let manager = OpenSCManagerW(PCWSTR::null(), PCWSTR::null(), SC_MANAGER_ALL_ACCESS)
            .map_err(|e| anyhow::anyhow!("OpenSCManager failed (administrator required?): {e}"))?;
        let service = OpenServiceW(manager, PCWSTR(service_w.as_ptr()), SERVICE_ALL_ACCESS)
            .map_err(|e| {
                let _ = CloseServiceHandle(manager);
                anyhow::anyhow!("cannot open service `{service_id}`: {e}")
            });
        let _ = CloseServiceHandle(manager);
        service
    }
}

#[cfg(test)]
mod info_flags_tests {
    use super::*;

    /// 0.1.3 regression: the delegated DACL carries only D:, and asking
    /// SetServiceObjectSecurity for the missing OWNER/GROUP components
    /// failed with ERROR_INVALID_PARAMETER (87).
    #[test]
    fn dacl_only_sddl_yields_dacl_flag() {
        let f = info_flags_for_sddl(
            "D:P(A;;GA;;;BA)(A;;GA;;;SY)(A;;RPWPLC;;;S-1-5-21-294617185-3689988605-2414484505-1001)",
        );
        assert!(!f.owner && !f.group && f.dacl);
    }

    #[test]
    fn full_sddl_yields_all_flags() {
        let f = info_flags_for_sddl("O:BAG:BAD:AI(A;;GA;;;BA)(A;;GA;;;SY)D:P(A;;RPWPLC;;;WD)");
        assert!(f.owner && f.group && f.dacl);
    }

    #[test]
    fn sacl_only_sddl_yields_no_flags() {
        // SACL-only: setting S: needs SE_SECURITY_NAME and is unsupported;
        // the caller rejects a flags-less descriptor.
        assert!(info_flags_for_sddl("S:").is_empty());
    }

    #[test]
    fn ace_contents_cannot_spoof_section_tags() {
        // "D:" appears inside the ACE, but the header "O:BAG:" carries no
        // DACL tag — only the header is scanned.
        let f = info_flags_for_sddl("O:BAG:(A;;RPWP;;;WD;D:not-a-section-tag)");
        assert!(f.owner && f.group && !f.dacl);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The generated DACL must be accepted by the real SDDL parser (the same
    /// one apply_security_descriptor feeds) — a typo'd right letter or SID
    /// placeholder would only explode at install time otherwise.
    fn assert_parses(sddl: &str) {
        let sddl_w: Vec<u16> = sddl.encode_utf16().chain([0]).collect();
        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                PCWSTR(sddl_w.as_ptr()),
                SDDL_REVISION_1,
                &mut descriptor,
                None,
            )
            .unwrap_or_else(|e| panic!("SDDL `{sddl}` rejected: {e}"));
            let _ = LocalFree(Some(HLOCAL(descriptor.0.cast())));
        }
    }

    #[test]
    fn delegated_sddl_shape() {
        assert_eq!(build_delegated_sddl(&[]), DELEGATED_BASE_SDDL);
        assert_eq!(
            build_delegated_sddl(&["S-1-5-32-544".into()]),
            "D:P(A;;GA;;;BA)(A;;GA;;;SY)(A;;CCLCSWLOCRRC;;;IU)(A;;CCLCSWLOCRRC;;;SU)(A;;RPWPLC;;;S-1-5-32-544)"
        );
        assert_eq!(
            build_delegated_sddl(&["S-1-5-32-544".into(), "S-1-1-0".into()])
                .matches("(A;;RPWPLC;;;")
                .count(),
            2
        );
        assert_parses(&build_delegated_sddl(&[
            "S-1-5-32-544".into(),
            "S-1-1-0".into(),
        ]));
    }

    /// Well-known account; also proves LookupAccountName/ConvertSidToStringSid
    /// work unelevated (validate resolves account names in a plain terminal).
    #[test]
    fn builtin_administrators_resolves_to_well_known_sid() {
        assert_eq!(
            account_to_sid_string("BUILTIN\\Administrators").unwrap(),
            "S-1-5-32-544"
        );
    }

    #[test]
    fn unknown_account_fails_named() {
        let err = account_to_sid_string("NO SUCH ACCOUNT rsw-test")
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("NO SUCH ACCOUNT rsw-test"),
            "error must name the account: {err}"
        );
    }
}
