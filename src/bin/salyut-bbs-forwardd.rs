use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    os::unix::{
        fs::PermissionsExt,
        net::{UnixListener, UnixStream},
    },
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use clap::Parser;
use salyut_bbs::forward_map::find_username;

const MAX_QUERY_BYTES: u64 = 512;

#[derive(Parser)]
#[command(version, about = "Resolve BBS authors from safe ~/.forward files")]
struct Arguments {
    #[arg(long, default_value = "/run/salyut-bbs/users/forward-map.sock")]
    socket: PathBuf,
    #[arg(long, default_value = "/etc/passwd")]
    passwd: PathBuf,
    #[arg(long)]
    resolve: Option<String>,
}

fn main() -> Result<()> {
    let arguments = Arguments::parse();
    if let Some(address) = arguments.resolve {
        let username = find_username(&address, &arguments.passwd)?
            .context("forwarding address is not registered")?;
        println!("{username}");
        return Ok(());
    }
    let listener = bind_listener(&arguments.socket)?;
    for connection in listener.incoming() {
        match connection {
            Ok(mut stream) => {
                if let Err(error) = handle(&mut stream, &arguments.passwd) {
                    eprintln!("forward lookup failed: {error:#}");
                    let _ = stream.write_all(b"ERROR\n");
                }
            }
            Err(error) => eprintln!("accept forward lookup: {error}"),
        }
    }
    Ok(())
}

fn bind_listener(path: &Path) -> Result<UnixListener> {
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| format!("remove stale {}", path.display()));
        }
    }
    let listener = UnixListener::bind(path).with_context(|| format!("bind {}", path.display()))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o660))
        .with_context(|| format!("set permissions on {}", path.display()))?;
    Ok(listener)
}

fn handle(stream: &mut UnixStream, passwd: &Path) -> Result<()> {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .context("set lookup read timeout")?;
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .context("set lookup write timeout")?;
    let mut query = String::new();
    BufReader::new(&mut *stream)
        .take(MAX_QUERY_BYTES + 1)
        .read_line(&mut query)
        .context("read forwarding address")?;
    if query.len() as u64 > MAX_QUERY_BYTES || !query.ends_with('\n') {
        bail!("invalid forwarding lookup request");
    }
    let address = query.trim_end_matches(['\r', '\n']);
    match find_username(address, passwd)? {
        Some(username) => writeln!(stream, "OK {username}").context("write forwarding user")?,
        None => stream
            .write_all(b"NOTFOUND\n")
            .context("write missing forwarding user")?,
    }
    Ok(())
}
