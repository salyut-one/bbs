use std::{
    ffi::CString,
    fs::File,
    io::{self, Read},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{ffi::OsStrExt, fs::MetadataExt},
    },
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use mailparse::MailAddr;

const MIN_UID: u32 = 1000;
const MAX_UID_EXCLUSIVE: u32 = 60000;
const MAX_FORWARD_BYTES: u64 = 64 * 1024;

struct Account {
    username: String,
    uid: u32,
    home: PathBuf,
}

pub fn find_username(address: &str, passwd: &Path) -> Result<Option<String>> {
    let address = normalize_address(address)?;
    let accounts = local_accounts(passwd)?;
    let mut matched = None;
    for account in accounts {
        let contents = match read_forward(&account) {
            Ok(Some(contents)) => contents,
            Ok(None) => continue,
            Err(error) => {
                eprintln!(
                    "ignoring unsafe .forward for {}: {error:#}",
                    account.username
                );
                continue;
            }
        };
        if !forward_addresses(&contents)
            .iter()
            .any(|candidate| candidate == &address)
        {
            continue;
        }
        if matched.is_some() {
            bail!("forwarding address belongs to more than one local account");
        }
        matched = Some(account.username);
    }
    Ok(matched)
}

pub fn normalize_address(address: &str) -> Result<String> {
    if address.len() > 320 || address.contains(['\r', '\n', '\0']) {
        bail!("invalid forwarding address");
    }
    let parsed = mailparse::addrparse(address).context("parse forwarding address")?;
    if parsed.len() != 1 {
        bail!("forwarding address must contain one mailbox");
    }
    let MailAddr::Single(single) = &parsed[0] else {
        bail!("forwarding address must not be a group");
    };
    let (local, domain) = single
        .addr
        .rsplit_once('@')
        .context("forwarding address has no domain")?;
    if local.is_empty() || domain.is_empty() {
        bail!("invalid forwarding address");
    }
    Ok(format!("{local}@{}", domain.trim_end_matches('.')).to_ascii_lowercase())
}

pub fn forward_addresses(contents: &str) -> Vec<String> {
    let mut addresses = Vec::new();
    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        for entry in split_entries(line) {
            let entry = entry.trim();
            if entry.is_empty()
                || entry.starts_with(['\\', '|', '/'])
                || entry
                    .get(..9)
                    .is_some_and(|prefix| prefix.eq_ignore_ascii_case(":include:"))
            {
                continue;
            }
            let Ok(parsed) = mailparse::addrparse(entry) else {
                continue;
            };
            for address in parsed.iter() {
                if let MailAddr::Single(single) = address
                    && let Ok(normalized) = normalize_address(&single.addr)
                {
                    addresses.push(normalized);
                }
            }
        }
    }
    addresses.sort();
    addresses.dedup();
    addresses
}

fn split_entries(line: &str) -> Vec<&str> {
    let mut entries = Vec::new();
    let mut start = 0;
    let mut quoted = false;
    let mut escaped = false;
    let mut angle_depth = 0_u32;
    let mut comment_depth = 0_u32;
    for (index, character) in line.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if quoted && character == '\\' {
            escaped = true;
            continue;
        }
        match character {
            '"' if comment_depth == 0 => quoted = !quoted,
            '(' if !quoted => comment_depth = comment_depth.saturating_add(1),
            ')' if !quoted => comment_depth = comment_depth.saturating_sub(1),
            '<' if !quoted && comment_depth == 0 => angle_depth = angle_depth.saturating_add(1),
            '>' if !quoted && comment_depth == 0 => angle_depth = angle_depth.saturating_sub(1),
            ',' if !quoted && comment_depth == 0 && angle_depth == 0 => {
                entries.push(&line[start..index]);
                start = index + character.len_utf8();
            }
            _ => {}
        }
    }
    entries.push(&line[start..]);
    entries
}

fn local_accounts(passwd: &Path) -> Result<Vec<Account>> {
    let contents =
        std::fs::read_to_string(passwd).with_context(|| format!("read {}", passwd.display()))?;
    let mut accounts = Vec::new();
    for line in contents.lines() {
        let fields: Vec<_> = line.split(':').collect();
        if fields.len() != 7 {
            continue;
        }
        let Ok(uid) = fields[2].parse::<u32>() else {
            continue;
        };
        if !(MIN_UID..MAX_UID_EXCLUSIVE).contains(&uid) {
            continue;
        }
        let username = fields[0];
        if username.is_empty()
            || !username
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
        {
            continue;
        }
        accounts.push(Account {
            username: username.to_owned(),
            uid,
            home: PathBuf::from(fields[5]),
        });
    }
    Ok(accounts)
}

fn read_forward(account: &Account) -> Result<Option<String>> {
    let home_path = CString::new(account.home.as_os_str().as_bytes())
        .context("home path contains a NUL byte")?;
    let home_fd = unsafe {
        libc::open(
            home_path.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if home_fd < 0 {
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::NotFound {
            return Ok(None);
        }
        return Err(error).with_context(|| format!("open {}", account.home.display()));
    }
    let home = unsafe { File::from_raw_fd(home_fd) };
    let metadata = home
        .metadata()
        .with_context(|| format!("inspect {}", account.home.display()))?;
    if !metadata.is_dir() || metadata.uid() != account.uid || metadata.mode() & 0o022 != 0 {
        bail!("unsafe home directory for {}", account.username);
    }

    let name = c".forward";
    let forward_fd = unsafe {
        libc::openat(
            home.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
        )
    };
    if forward_fd < 0 {
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::NotFound {
            return Ok(None);
        }
        return Err(error).with_context(|| format!("open {}/.forward", account.home.display()));
    }
    let forward = unsafe { File::from_raw_fd(forward_fd) };
    let metadata = forward
        .metadata()
        .with_context(|| format!("inspect {}/.forward", account.home.display()))?;
    if !metadata.is_file() || metadata.uid() != account.uid || metadata.mode() & 0o022 != 0 {
        bail!("unsafe .forward file for {}", account.username);
    }
    if metadata.len() > MAX_FORWARD_BYTES {
        bail!(".forward file is too large for {}", account.username);
    }
    let mut contents = String::new();
    forward
        .take(MAX_FORWARD_BYTES + 1)
        .read_to_string(&mut contents)
        .with_context(|| format!("read {}/.forward", account.home.display()))?;
    if contents.len() as u64 > MAX_FORWARD_BYTES {
        bail!(".forward file is too large for {}", account.username);
    }
    Ok(Some(contents))
}

#[cfg(test)]
mod tests {
    use super::{forward_addresses, normalize_address};

    #[test]
    fn parses_plain_forwarding_addresses_and_ignores_actions() {
        let contents = "# keep a local copy\n\
            External Person <Person@Example.COM>, \\alice\n\
            |/usr/local/bin/filter\n\
            /tmp/archive\n\
            :include:/tmp/list\n\
            second@example.net\n";
        assert_eq!(
            forward_addresses(contents),
            vec!["person@example.com", "second@example.net"]
        );
    }

    #[test]
    fn address_normalization_requires_one_mailbox() {
        assert_eq!(
            normalize_address("Person <Person@Example.COM.>").unwrap(),
            "person@example.com"
        );
        assert!(normalize_address("one@example.com, two@example.com").is_err());
        assert!(normalize_address("Undisclosed:;").is_err());
    }
}
