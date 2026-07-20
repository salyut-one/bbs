use std::{
    ffi::{CStr, CString},
    io,
    mem::MaybeUninit,
    os::fd::{AsRawFd, RawFd},
    os::unix::net::UnixStream,
    ptr,
};

use anyhow::{Context, Result, bail};

use crate::db::MailRecipient;

pub const MAIL_UID_MIN: u32 = 1000;
pub const MAIL_UID_MAX_EXCLUSIVE: u32 = 60_000;

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

pub fn account_by_username(username: &str) -> Result<Account> {
    let username = CString::new(username).context("Unix username contains a NUL byte")?;
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
        libc::getpwnam_r(
            username.as_ptr(),
            entry.as_mut_ptr(),
            buffer.as_mut_ptr().cast(),
            buffer.len(),
            &mut result,
        )
    };
    if status != 0 {
        return Err(io::Error::from_raw_os_error(status)).context("resolve Unix username");
    }
    if result.is_null() {
        bail!("no Unix user exists for authenticated username");
    }
    // SAFETY: a successful getpwnam_r returned a valid entry backed by buffer.
    account(unsafe { (*result).pw_uid })
}

pub fn mail_eligible(uid: u32) -> bool {
    (MAIL_UID_MIN..MAIL_UID_MAX_EXCLUSIVE).contains(&uid)
}

pub fn mail_recipients() -> Result<Vec<MailRecipient>> {
    let passwd = std::fs::read_to_string("/etc/passwd").context("read /etc/passwd")?;
    let mut recipients = passwd
        .lines()
        .filter_map(|line| {
            let mut fields = line.split(':');
            let username = fields.next()?;
            fields.next()?;
            let uid = fields.next()?.parse::<u32>().ok()?;
            (mail_eligible(uid) && valid_mail_username(username)).then(|| MailRecipient {
                uid,
                username: username.to_owned(),
            })
        })
        .collect::<Vec<_>>();
    recipients.sort_by(|left, right| {
        left.uid
            .cmp(&right.uid)
            .then_with(|| left.username.cmp(&right.username))
    });
    recipients.dedup_by_key(|recipient| recipient.uid);
    Ok(recipients)
}

fn valid_mail_username(username: &str) -> bool {
    !username.is_empty()
        && username
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
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
    use super::{account, account_by_username, mail_eligible};

    #[test]
    fn mail_uid_range_includes_1000_and_excludes_60000() {
        assert!(!mail_eligible(999));
        assert!(mail_eligible(1000));
        assert!(mail_eligible(59_999));
        assert!(!mail_eligible(60_000));
    }

    #[test]
    fn current_account_includes_its_primary_group() {
        // SAFETY: getuid has no preconditions.
        let account = account(unsafe { libc::getuid() }).unwrap();
        assert!(!account.groups.is_empty());
    }

    #[test]
    fn current_account_resolves_by_username() {
        // SAFETY: getuid has no preconditions.
        let by_uid = account(unsafe { libc::getuid() }).unwrap();
        let by_name = account_by_username(&by_uid.username).unwrap();
        assert_eq!(by_name.uid, by_uid.uid);
        assert_eq!(by_name.username, by_uid.username);
        assert_eq!(by_name.groups, by_uid.groups);
    }
}
