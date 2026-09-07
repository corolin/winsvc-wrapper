//! Service account helpers: grant the "Log on as a service" right
//! (SeServiceLogonRight) via LSA.

use windows::Win32::Foundation::{ERROR_INSUFFICIENT_BUFFER, WIN32_ERROR};
use windows::Win32::Security::Authentication::Identity::SE_SERVICE_LOGON_NAME;
use windows::Win32::Security::Authentication::Identity::{
    LSA_OBJECT_ATTRIBUTES, LSA_UNICODE_STRING, LsaAddAccountRights, LsaClose, LsaOpenPolicy,
    POLICY_CREATE_ACCOUNT, POLICY_LOOKUP_NAMES,
};
use windows::Win32::Security::{LookupAccountNameW, PSID, SID_NAME_USE};
use windows::core::{PCWSTR, PWSTR};

use crate::config::AccountConfig;

/// Grants SeServiceLogonRight to the configured account, best effort.
///
/// Returns Ok with a skip note for virtual accounts (they already have the
/// right), and Err with a human-readable message when the grant fails — the
/// installer reports it but still leaves the service installed.
pub fn ensure_logon_as_service(acct: &AccountConfig) -> anyhow::Result<String> {
    if acct
        .username
        .eq_ignore_ascii_case("NT AUTHORITY\\LocalService")
        || acct
            .username
            .eq_ignore_ascii_case("NT AUTHORITY\\NetworkService")
        || acct.username.eq_ignore_ascii_case("LocalSystem")
    {
        return Ok("virtual account already holds the logon-as-service right".into());
    }
    if !acct.allow_logon_as_service {
        return Ok("allow_logon_as_service = false; skipping the right grant".into());
    }

    unsafe {
        // 1) Account name -> SID (two-pass sizing).
        let (mut system, mut name) = split_account(&acct.username);
        let system_ptr = PCWSTR(system.as_mut().map_or(std::ptr::null(), |v| v.as_mut_ptr()));
        let name_ptr = PCWSTR(name.as_mut_ptr());
        let mut sid_len = 0u32;
        let mut domain_len = 0u32;
        let mut use_ = SID_NAME_USE::default();
        // First pass fails with an expected buffer error just to size the SID.
        let _ = LookupAccountNameW(
            system_ptr,
            name_ptr,
            None,
            &mut sid_len,
            None,
            &mut domain_len,
            &mut use_,
        );
        if sid_len == 0 {
            anyhow::bail!(
                "cannot resolve account `{}` (LookupAccountNameW failed)",
                acct.username
            );
        }
        let mut sid: Vec<u8> = vec![0; sid_len as usize];
        let mut domain: Vec<u16> = vec![0; domain_len.max(1) as usize];
        LookupAccountNameW(
            system_ptr,
            name_ptr,
            Some(PSID(sid.as_mut_ptr() as *mut _)),
            &mut sid_len,
            Some(PWSTR(domain.as_mut_ptr())),
            &mut domain_len,
            &mut use_,
        )
        .map_err(|e| anyhow::anyhow!("cannot resolve account `{}`: {e}", acct.username))?;

        // 2) LSA: open the local policy and add the right.
        let attributes = LSA_OBJECT_ATTRIBUTES::default();
        let mut policy = Default::default();
        let status = LsaOpenPolicy(
            None,
            &attributes,
            (POLICY_LOOKUP_NAMES | POLICY_CREATE_ACCOUNT) as u32,
            &mut policy,
        );
        if status.is_err() {
            anyhow::bail!("LsaOpenPolicy failed: {status:?}");
        }
        let right = lsa_unicode_string(SE_SERVICE_LOGON_NAME);
        let status = LsaAddAccountRights(policy, PSID(sid.as_ptr() as *mut _), &[right]);
        let _ = LsaClose(policy);
        if status.is_err() {
            anyhow::bail!("LsaAddAccountRights(SeServiceLogonRight) failed: {status:?}");
        }
    }
    Ok(format!("granted SeServiceLogonRight to {}", acct.username))
}

/// Splits `DOMAIN\user` into wide buffers (`None` for the local machine on
/// bare names), both NUL-terminated for the W APIs.
fn split_account(username: &str) -> (Option<Vec<u16>>, Vec<u16>) {
    let mut system = None;
    let account;
    if let Some((dom, user)) = username.split_once('\\') {
        let mut dom_w: Vec<u16> = dom.encode_utf16().collect();
        dom_w.push(0);
        system = Some(dom_w);
        account = user.to_string();
    } else {
        account = username.to_string();
    }
    let mut name_w: Vec<u16> = account.encode_utf16().collect();
    name_w.push(0);
    (system, name_w)
}

fn lsa_unicode_string(s: PCWSTR) -> LSA_UNICODE_STRING {
    // SAFETY: the string is a static (SE_SERVICE_LOGON_NAME); length in bytes
    // excludes the NUL terminator per LSA_UNICODE_STRING contract.
    unsafe {
        let mut len = 0usize;
        while *s.0.add(len) != 0 {
            len += 1;
        }
        LSA_UNICODE_STRING {
            Length: (len * 2) as u16,
            MaximumLength: ((len + 1) * 2) as u16,
            Buffer: PWSTR(s.0 as *mut u16),
        }
    }
}

/// Silence unused-import warning for the constant used above.
#[allow(unused)]
fn _error_code() -> WIN32_ERROR {
    ERROR_INSUFFICIENT_BUFFER
}
