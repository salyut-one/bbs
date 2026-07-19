use std::path::PathBuf;

#[cfg(target_os = "macos")]
fn runtime_directory() -> PathBuf {
    std::env::var_os("TMPDIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("salyut-bbs")
}

#[cfg(target_os = "macos")]
pub fn read_write_socket() -> PathBuf {
    runtime_directory().join("users.sock")
}

#[cfg(not(target_os = "macos"))]
pub fn read_write_socket() -> PathBuf {
    PathBuf::from("/run/salyut-bbs/users/salyut.sock")
}

#[cfg(target_os = "macos")]
pub fn database() -> PathBuf {
    runtime_directory().join("posts.sqlite3")
}

#[cfg(not(target_os = "macos"))]
pub fn database() -> PathBuf {
    PathBuf::from("/var/lib/salyut-bbs/posts.sqlite3")
}
