use std::{
    ffi::{CStr, CString},
    io,
    mem::MaybeUninit,
    os::fd::{AsRawFd, RawFd},
    os::unix::net::UnixStream,
    ptr,
};

use anyhow::{Context, Result, bail};

#[derive(Debug, Clone)]
pub struct Account {
    pub uid: u32,
    pub username: String,
    pub groups: Vec<String>,
}

pub fn uid(stream: &UnixStream) -> io::Result<u32> {
    peer_uid(stream.as_raw_fd())
}

#[cfg(target_os = "macos")]
fn peer_uid(fd: RawFd) -> io::Result<u32> {
    let mut uid: libc::uid_t = 0;
    let mut gid: libc::gid_t = 0;
    // SAFETY: fd is a live Unix stream and both output pointers are valid.
    let result = unsafe { libc::getpeereid(fd, &mut uid, &mut gid) };
    if result == 0 {
        Ok(uid)
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(target_os = "linux")]
fn peer_uid(fd: RawFd) -> io::Result<u32> {
    let mut credentials = MaybeUninit::<libc::ucred>::uninit();
    let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: credentials has enough space for ucred and length describes it.
    let result = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            credentials.as_mut_ptr().cast(),
            &mut length,
        )
    };
    if result == 0 {
        // SAFETY: successful getsockopt initialized credentials.
        Ok(unsafe { credentials.assume_init() }.uid)
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
compile_error!("salyut-bbs requires a platform with Unix peer credential support");

pub fn account(uid: u32) -> Result<Account> {
    let buffer_size = unsafe { libc::sysconf(libc::_SC_GETPW_R_SIZE_MAX) };
    let buffer_size = if buffer_size <= 0 {
        16 * 1024
    } else {
        buffer_size as usize
    };
    let mut buffer = vec![0_u8; buffer_size];
    let mut entry = MaybeUninit::<libc::passwd>::uninit();
    let mut result = ptr::null_mut();

    // SAFETY: all buffers and output pointers are valid for the duration of the call.
    let status = unsafe {
        libc::getpwuid_r(
            uid,
            entry.as_mut_ptr(),
            buffer.as_mut_ptr().cast(),
            buffer.len(),
            &mut result,
        )
    };
    if status != 0 {
        return Err(io::Error::from_raw_os_error(status))
            .with_context(|| format!("resolve Unix user for uid {uid}"));
    }
    if result.is_null() {
        bail!("no Unix user exists for uid {uid}");
    }

    // SAFETY: a successful getpwuid_r returned a valid entry backed by buffer.
    let name = unsafe { CStr::from_ptr((*result).pw_name) };
    let name = name
        .to_str()
        .context("Unix username is not valid UTF-8")?
        .to_owned();
    if name.is_empty() {
        bail!("Unix user for uid {uid} has an empty username");
    }
    // SAFETY: result points at the initialized passwd entry backed by buffer.
    let primary_gid = unsafe { (*result).pw_gid };
    let groups = groups_for_user(&name, primary_gid)?;
    Ok(Account {
        uid,
        username: name,
        groups,
    })
}

fn groups_for_user(username: &str, primary_gid: libc::gid_t) -> Result<Vec<String>> {
    let username = CString::new(username).context("Unix username contains a NUL byte")?;
    let mut gids = group_ids(&username, primary_gid)?;
    gids.sort_unstable();
    gids.dedup();
    gids.into_iter().map(group_name).collect()
}

#[cfg(target_os = "macos")]
fn group_ids(username: &CString, primary_gid: libc::gid_t) -> Result<Vec<libc::gid_t>> {
    let primary_gid =
        libc::c_int::try_from(primary_gid).context("primary group id exceeds c_int")?;
    let mut count: libc::c_int = 32;
    loop {
        let mut gids = vec![0 as libc::c_int; count as usize];
        let mut returned = count;
        // SAFETY: gids has capacity for count entries and all pointers are valid.
        let status = unsafe {
            libc::getgrouplist(
                username.as_ptr(),
                primary_gid,
                gids.as_mut_ptr(),
                &mut returned,
            )
        };
        if status >= 0 {
            gids.truncate(returned as usize);
            return gids
                .into_iter()
                .map(|gid| libc::gid_t::try_from(gid).context("group id is negative"))
                .collect();
        }
        if returned <= count {
            bail!("could not resolve Unix group list");
        }
        count = returned;
    }
}

#[cfg(target_os = "linux")]
fn group_ids(username: &CString, primary_gid: libc::gid_t) -> Result<Vec<libc::gid_t>> {
    let mut count: libc::c_int = 32;
    loop {
        let mut gids = vec![0 as libc::gid_t; count as usize];
        let mut returned = count;
        // SAFETY: gids has capacity for count entries and all pointers are valid.
        let status = unsafe {
            libc::getgrouplist(
                username.as_ptr(),
                primary_gid,
                gids.as_mut_ptr(),
                &mut returned,
            )
        };
        if status >= 0 {
            gids.truncate(returned as usize);
            return Ok(gids);
        }
        if returned <= count {
            bail!("could not resolve Unix group list");
        }
        count = returned;
    }
}

fn group_name(gid: libc::gid_t) -> Result<String> {
    let buffer_size = unsafe { libc::sysconf(libc::_SC_GETGR_R_SIZE_MAX) };
    let buffer_size = if buffer_size <= 0 {
        16 * 1024
    } else {
        buffer_size as usize
    };
    let mut buffer = vec![0_u8; buffer_size];
    let mut entry = MaybeUninit::<libc::group>::uninit();
    let mut result = ptr::null_mut();
    // SAFETY: all buffers and output pointers are valid for the duration of the call.
    let status = unsafe {
        libc::getgrgid_r(
            gid,
            entry.as_mut_ptr(),
            buffer.as_mut_ptr().cast(),
            buffer.len(),
            &mut result,
        )
    };
    if status != 0 {
        return Err(io::Error::from_raw_os_error(status))
            .with_context(|| format!("resolve Unix group for gid {gid}"));
    }
    if result.is_null() {
        bail!("no Unix group exists for gid {gid}");
    }
    // SAFETY: a successful getgrgid_r returned a valid entry backed by buffer.
    let name = unsafe { CStr::from_ptr((*result).gr_name) };
    Ok(name
        .to_str()
        .context("Unix group name is not valid UTF-8")?
        .to_owned())
}

#[cfg(test)]
mod tests {
    #[test]
    fn current_account_includes_its_primary_group() {
        // SAFETY: getuid has no preconditions.
        let account = super::account(unsafe { libc::getuid() }).unwrap();
        assert!(!account.groups.is_empty());
    }
}
