//! Shared directory mapping (WinSW `sharedDirectoryMapping`): connects a
//! drive letter to a UNC share before the child starts.

use windows::Win32::Foundation::NO_ERROR;
use windows::Win32::NetworkManagement::WNet::{
    NETRESOURCEW, RESOURCETYPE_DISK, WNetAddConnection2W,
};
use windows::core::PWSTR;

use crate::config::MapDriveConfig;
use crate::logging::LogSink;

pub fn map_drives(mappings: &[MapDriveConfig], sink: &LogSink) {
    for m in mappings {
        let mut label: Vec<u16> = m.label.encode_utf16().chain([0]).collect();
        let mut unc: Vec<u16> = m.unc_path.encode_utf16().chain([0]).collect();
        let resource = NETRESOURCEW {
            dwType: RESOURCETYPE_DISK,
            lpLocalName: PWSTR(label.as_mut_ptr()),
            lpRemoteName: PWSTR(unc.as_mut_ptr()),
            ..Default::default()
        };
        let result = unsafe {
            WNetAddConnection2W(&resource, PWSTR::null(), PWSTR::null(), Default::default())
        };
        if result == NO_ERROR {
            sink.info(&format!("mapped {} -> {}", m.label, m.unc_path));
        } else {
            sink.warn(&format!(
                "could not map {} -> {} (WNetAddConnection2W error {})",
                m.label, m.unc_path, result.0
            ));
        }
    }
}
