//! Windows security helpers for the named pipe: the current user's SID,
//! a security descriptor that admits only that user, and the client-side
//! check that the pipe was created by this user (not squatted by another
//! account that created the name first).

use std::io;
use std::os::windows::io::AsRawHandle;
use std::ptr::null_mut;
use windows_sys::Win32::Foundation::{CloseHandle, LocalFree, HANDLE};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, GetSecurityInfo, SDDL_REVISION_1,
    SE_KERNEL_OBJECT,
};
use windows_sys::Win32::Security::{
    GetTokenInformation, TokenUser, OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_QUERY,
    TOKEN_USER,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

/// Well-known SID of BUILTIN\Administrators: the default owner of objects
/// created by an elevated administrator (agent started with `-Elevated`).
const ADMINISTRATORS: &str = "S-1-5-32-544";

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Converts a SID to its string form (`S-1-5-21-…`).
unsafe fn sid_string(sid: *mut core::ffi::c_void) -> io::Result<String> {
    let mut s = null_mut();
    if ConvertSidToStringSidW(sid, &mut s) == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut n = 0;
    while *s.add(n) != 0 {
        n += 1;
    }
    let out = String::from_utf16_lossy(std::slice::from_raw_parts(s, n));
    LocalFree(s as _);
    Ok(out)
}

/// SID of the user this process runs as.
pub fn current_user_sid() -> io::Result<String> {
    unsafe {
        let mut token: HANDLE = null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut len = 0u32;
        GetTokenInformation(token, TokenUser, null_mut(), 0, &mut len);
        // u64 storage keeps TOKEN_USER suitably aligned.
        let mut buf = vec![0u64; (len as usize).div_ceil(8).max(1)];
        let ok = GetTokenInformation(token, TokenUser, buf.as_mut_ptr() as _, len, &mut len);
        CloseHandle(token);
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        let tu = &*(buf.as_ptr() as *const TOKEN_USER);
        sid_string(tu.User.Sid)
    }
}

/// Security attributes for pipe instances: full access for the current
/// user only (no Everyone/Anonymous read access as in the default pipe
/// DACL). Keep it alive while pipe instances are created.
pub struct OwnerOnly {
    sd: PSECURITY_DESCRIPTOR,
    attrs: SECURITY_ATTRIBUTES,
}

// The descriptor is immutable after creation and only read by the kernel.
unsafe impl Send for OwnerOnly {}
unsafe impl Sync for OwnerOnly {}

impl OwnerOnly {
    pub fn new() -> io::Result<OwnerOnly> {
        let sid = current_user_sid()?;
        // Protected DACL with one entry: GENERIC_ALL for the user. The
        // medium mandatory label lets a non-elevated tray talk to an agent
        // started elevated; creating it can fail where labels are not
        // supported, then fall back to the plain DACL.
        let with_label = format!("D:P(A;;GA;;;{sid})S:(ML;;NW;;;ME)");
        let plain = format!("D:P(A;;GA;;;{sid})");
        let sd = Self::descriptor(&with_label).or_else(|_| Self::descriptor(&plain))?;
        let attrs = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: sd,
            bInheritHandle: 0,
        };
        Ok(OwnerOnly { sd, attrs })
    }

    fn descriptor(sddl: &str) -> io::Result<PSECURITY_DESCRIPTOR> {
        let w = wide(sddl);
        let mut sd: PSECURITY_DESCRIPTOR = null_mut();
        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(w.as_ptr(), SDDL_REVISION_1, &mut sd, null_mut())
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(sd)
    }

    /// Pointer for `CreateNamedPipeW` (`tokio`'s
    /// `create_with_security_attributes_raw`).
    pub fn as_ptr(&mut self) -> *mut core::ffi::c_void {
        &mut self.attrs as *mut SECURITY_ATTRIBUTES as _
    }
}

impl Drop for OwnerOnly {
    fn drop(&mut self) {
        unsafe { LocalFree(self.sd as _) };
    }
}

/// Owner SID of a kernel object (pipe handle).
pub fn owner_sid(h: &impl AsRawHandle) -> io::Result<String> {
    unsafe {
        let mut owner = null_mut();
        let mut sd: PSECURITY_DESCRIPTOR = null_mut();
        let rc = GetSecurityInfo(
            h.as_raw_handle() as HANDLE,
            SE_KERNEL_OBJECT,
            OWNER_SECURITY_INFORMATION,
            &mut owner,
            null_mut(),
            null_mut(),
            null_mut(),
            &mut sd,
        );
        if rc != 0 {
            return Err(io::Error::from_raw_os_error(rc as i32));
        }
        let s = sid_string(owner);
        LocalFree(sd as _);
        s
    }
}

/// Refuses a pipe that another (non-admin) user created under our name.
pub fn check_pipe_owner(f: &std::fs::File) -> io::Result<()> {
    let owner = owner_sid(f)?;
    let me = current_user_sid()?;
    if owner == me || owner == ADMINISTRATORS {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("the agent pipe belongs to {owner}, not to this user ({me})"),
        ))
    }
}
