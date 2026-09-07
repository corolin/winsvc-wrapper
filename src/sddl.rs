//! SDDL security descriptors applied to the service object.

use windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{
    DACL_SECURITY_INFORMATION, GROUP_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION,
    PSECURITY_DESCRIPTOR,
};
use windows::Win32::System::Services::{
    CloseServiceHandle, OpenSCManagerW, OpenServiceW, SC_HANDLE, SC_MANAGER_ALL_ACCESS,
    SERVICE_ALL_ACCESS, SetServiceObjectSecurity,
};
use windows::core::PCWSTR;

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
