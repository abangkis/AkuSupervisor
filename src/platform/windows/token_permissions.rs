#![allow(unsafe_code)]

use std::ffi::c_void;
use std::fmt;
use std::io;
use std::mem::size_of;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::ptr;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, LocalFree};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    SE_FILE_OBJECT, SetNamedSecurityInfoW,
};
use windows_sys::Win32::Security::{
    ACL, DACL_SECURITY_INFORMATION, GetSecurityDescriptorDacl, GetTokenInformation,
    PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, TOKEN_QUERY, TOKEN_USER, TokenUser,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

/// Replaces the token file DACL with one protected full-control entry for the
/// user running `AkuSupervisor`.
///
/// # Errors
///
/// Returns the failing Windows security stage without exposing token content.
pub fn harden_runtime_token_permissions(path: &Path) -> Result<(), TokenPermissionError> {
    let sid = current_user_sid_string().map_err(TokenPermissionError::CurrentUser)?;
    let descriptor = wide(&format!("D:P(A;;FA;;;{sid})"));
    let mut security_descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();
    let converted = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            descriptor.as_ptr(),
            SDDL_REVISION_1,
            &raw mut security_descriptor,
            ptr::null_mut(),
        )
    };
    if converted == 0 {
        return Err(TokenPermissionError::BuildDescriptor(
            io::Error::last_os_error(),
        ));
    }
    let _descriptor = LocalAllocation(security_descriptor);
    let mut dacl_present = 0;
    let mut dacl_defaulted = 0;
    let mut dacl: *mut ACL = ptr::null_mut();
    if unsafe {
        GetSecurityDescriptorDacl(
            security_descriptor,
            &raw mut dacl_present,
            &raw mut dacl,
            &raw mut dacl_defaulted,
        )
    } == 0
        || dacl_present == 0
        || dacl.is_null()
    {
        return Err(TokenPermissionError::BuildDescriptor(
            io::Error::last_os_error(),
        ));
    }
    let path_wide = path
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let result = unsafe {
        SetNamedSecurityInfoW(
            path_wide.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            ptr::null_mut(),
            ptr::null_mut(),
            dacl,
            ptr::null_mut(),
        )
    };
    if result != 0 {
        return Err(TokenPermissionError::Apply {
            path: path.to_owned(),
            source: io::Error::from_raw_os_error(i32::try_from(result).unwrap_or(i32::MAX)),
        });
    }
    Ok(())
}

fn current_user_sid_string() -> io::Result<String> {
    let mut token: HANDLE = ptr::null_mut();
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &raw mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let _token = OwnedHandle(token);
    let mut required = 0_u32;
    unsafe {
        GetTokenInformation(token, TokenUser, ptr::null_mut(), 0, &raw mut required);
    }
    if required == 0 {
        return Err(io::Error::last_os_error());
    }
    let word_size = u32::try_from(size_of::<usize>()).expect("usize width fits u32");
    let mut buffer = vec![0_usize; required.div_ceil(word_size) as usize];
    if unsafe {
        GetTokenInformation(
            token,
            TokenUser,
            buffer.as_mut_ptr().cast::<c_void>(),
            required,
            &raw mut required,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let token_user = unsafe { &*buffer.as_ptr().cast::<TOKEN_USER>() };
    let mut sid_text = ptr::null_mut();
    if unsafe { ConvertSidToStringSidW(token_user.User.Sid, &raw mut sid_text) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let _sid_text = LocalAllocation(sid_text.cast::<c_void>());
    let length = (0..256)
        .find(|index| unsafe { *sid_text.add(*index) } == 0)
        .ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "Windows SID text is too long")
        })?;
    Ok(String::from_utf16_lossy(unsafe {
        std::slice::from_raw_parts(sid_text, length)
    }))
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}

struct OwnedHandle(HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}

struct LocalAllocation(*mut c_void);

impl Drop for LocalAllocation {
    fn drop(&mut self) {
        unsafe {
            LocalFree(self.0);
        }
    }
}

#[derive(Debug)]
pub enum TokenPermissionError {
    CurrentUser(io::Error),
    BuildDescriptor(io::Error),
    Apply { path: PathBuf, source: io::Error },
}

impl fmt::Display for TokenPermissionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CurrentUser(source) => {
                write!(
                    formatter,
                    "failed to resolve current Windows user SID: {source}"
                )
            }
            Self::BuildDescriptor(source) => {
                write!(formatter, "failed to build protected token DACL: {source}")
            }
            Self::Apply { path, source } => write!(
                formatter,
                "failed to restrict token permissions on {}: {source}",
                path.display()
            ),
        }
    }
}

impl std::error::Error for TokenPermissionError {}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::mem::size_of;
    use std::os::windows::ffi::OsStrExt;
    use std::ptr;

    use windows_sys::Win32::Security::Authorization::{GetNamedSecurityInfoW, SE_FILE_OBJECT};
    use windows_sys::Win32::Security::{
        ACL, ACL_SIZE_INFORMATION, AclSizeInformation, DACL_SECURITY_INFORMATION,
        GetAclInformation, GetSecurityDescriptorControl, PSECURITY_DESCRIPTOR, SE_DACL_PROTECTED,
    };

    use super::{LocalAllocation, harden_runtime_token_permissions};

    #[test]
    fn current_user_can_still_read_a_hardened_token_file() {
        let directory =
            std::env::temp_dir().join(format!("aku-supervisor-token-acl-{}", std::process::id()));
        fs::create_dir_all(&directory).expect("create ACL test directory");
        let path = directory.join("control-token");
        fs::write(&path, b"test-token\n").expect("write token fixture");

        harden_runtime_token_permissions(&path).expect("apply current-user-only ACL");

        assert_eq!(
            fs::read_to_string(&path).expect("read hardened file"),
            "test-token\n"
        );
        let (protected, ace_count) = inspect_dacl(&path);
        assert!(protected);
        assert_eq!(ace_count, 1);
        fs::remove_dir_all(directory).ok();
    }

    fn inspect_dacl(path: &std::path::Path) -> (bool, u32) {
        let path = path
            .as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect::<Vec<_>>();
        let mut dacl: *mut ACL = ptr::null_mut();
        let mut descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();
        let result = unsafe {
            GetNamedSecurityInfoW(
                path.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                ptr::null_mut(),
                ptr::null_mut(),
                &raw mut dacl,
                ptr::null_mut(),
                &raw mut descriptor,
            )
        };
        assert_eq!(result, 0, "read hardened DACL");
        let _descriptor = LocalAllocation(descriptor);
        let mut control = 0_u16;
        let mut revision = 0_u32;
        assert_ne!(
            unsafe {
                GetSecurityDescriptorControl(descriptor, &raw mut control, &raw mut revision)
            },
            0
        );
        let mut size = ACL_SIZE_INFORMATION::default();
        assert_ne!(
            unsafe {
                GetAclInformation(
                    dacl,
                    (&raw mut size).cast(),
                    u32::try_from(size_of::<ACL_SIZE_INFORMATION>()).expect("ACL size fits u32"),
                    AclSizeInformation,
                )
            },
            0
        );
        (control & SE_DACL_PROTECTED != 0, size.AceCount)
    }
}
