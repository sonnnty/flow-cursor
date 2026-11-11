//! Win32 helpers (implemented in a later step).

pub fn shell_open(_target: &str) -> std::io::Result<()> {
    Err(std::io::Error::other("not implemented yet"))
}
