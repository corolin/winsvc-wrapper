//! SDDL security descriptors applied to the service object.

use windows::Win32::Foundation::{HLOCAL, LocalFree};
use windows::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{
    DACL_SECURITY_INFORMATION, GROUP_SECURITY_INFORMATION, LookupAccountNameW,
    OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, SID_NAME_USE,
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

        let service = open_service(service_id)?;
        let info =
            OWNER_SECURITY_INFORMATION | GROUP_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION;
        let result = SetServiceObjectSecurity(service, info, descriptor)
            .map_err(|e| anyhow::anyhow!("SetServiceObjectSecurity failed: {e}"));
        let _ = CloseServiceHandle(service);
        // The self-relative descriptor is heap-allocated by the API; free it.
        if result.is_ok() {
            let _ = windows::Win32::Foundation::LocalFree(Some(
                windows::Win32::Foundation::HLOCAL(descriptor.0 as _),
            ));
        }
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
