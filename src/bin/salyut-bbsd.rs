use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    os::unix::{fs::PermissionsExt, net::UnixListener},
    path::PathBuf,
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};

use anyhow::{Context, Result};
use clap::Parser;
use salyut_bbs::{
    db::Database,
    peer,
    protocol::{Board, BoardKind, ErrorCode, Request, Response},
};

#[derive(Parser)]
#[command(version, about = "salyut.one BBS daemon")]
struct Arguments {
    #[arg(long, default_value_os_t = salyut_bbs::paths::read_write_socket())]
    socket: PathBuf,
    #[arg(long, default_value_os_t = salyut_bbs::paths::read_only_socket())]
    read_only_socket: PathBuf,
    #[arg(long, default_value_os_t = salyut_bbs::paths::database())]
    database: PathBuf,
    #[arg(long, default_value = "0660", value_parser = parse_mode)]
    socket_mode: u32,
    #[arg(long, default_value = "0660", value_parser = parse_mode)]
    read_only_socket_mode: u32,
}

fn parse_mode(value: &str) -> Result<u32, String> {
    u32::from_str_radix(value, 8).map_err(|_| "mode must be an octal number".to_owned())
}

fn main() -> Result<()> {
    let arguments = Arguments::parse();
    if let Some(parent) = arguments.socket.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create socket directory {}", parent.display()))?;
    }
    if let Some(parent) = arguments.read_only_socket.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create socket directory {}", parent.display()))?;
    }
    if let Some(parent) = arguments.database.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create database directory {}", parent.display()))?;
    }

    let database = Arc::new(Mutex::new(Database::open(&arguments.database)?));
    let read_write_listener = bind_socket(&arguments.socket, arguments.socket_mode)?;
    let read_only_listener =
        bind_socket(&arguments.read_only_socket, arguments.read_only_socket_mode)?;
    eprintln!(
        "salyut-bbsd listening on {} (read/write) and {} (read-only)",
        arguments.socket.display(),
        arguments.read_only_socket.display()
    );

    let read_database = Arc::clone(&database);
    thread::spawn(move || accept_connections(read_only_listener, read_database, true));
    accept_connections(read_write_listener, database, false);
    Ok(())
}

fn bind_socket(path: &std::path::Path, mode: u32) -> Result<UnixListener> {
    if path.exists() {
        fs::remove_file(path).with_context(|| format!("remove stale socket {}", path.display()))?;
    }
    let listener =
        UnixListener::bind(path).with_context(|| format!("bind Unix socket {}", path.display()))?;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    Ok(listener)
}

fn accept_connections(listener: UnixListener, database: Arc<Mutex<Database>>, read_only: bool) {
    for connection in listener.incoming() {
        match connection {
            Ok(stream) => {
                let database = Arc::clone(&database);
                thread::spawn(move || {
                    if let Err(error) = serve_client(stream, database, read_only) {
                        eprintln!("client error: {error:#}");
                    }
                });
            }
            Err(error) => eprintln!("accept error: {error}"),
        }
    }
}

fn serve_client(
    mut stream: std::os::unix::net::UnixStream,
    database: Arc<Mutex<Database>>,
    read_only: bool,
) -> Result<()> {
    const MAX_REQUEST_BYTES: u64 = 512 * 1024;

    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    stream.set_write_timeout(Some(Duration::from_secs(10)))?;
    let uid = peer::uid(&stream).context("read peer credentials")?;
    let account = peer::account(uid)?;
    let reader_stream = stream.try_clone()?;
    let mut reader = BufReader::new(reader_stream).take(MAX_REQUEST_BYTES + 1);
    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        return Ok(());
    }
    let response = if line.len() as u64 > MAX_REQUEST_BYTES {
        error(ErrorCode::BadRequest, "request is too large")
    } else {
        match serde_json::from_str::<Request>(&line) {
            Ok(request) if read_only && request.is_mutating() => {
                error(ErrorCode::Forbidden, "this socket is read-only")
            }
            Ok(request) => dispatch(&database, &account, request),
            Err(cause) => Response::Error {
                code: ErrorCode::BadRequest,
                message: format!("invalid request: {cause}"),
            },
        }
    };
    serde_json::to_writer(&mut stream, &response)?;
    stream.write_all(b"\n")?;
    stream.flush()?;
    Ok(())
}

fn dispatch(database: &Mutex<Database>, account: &peer::Account, request: Request) -> Response {
    let result = (|| -> Result<Response> {
        let mut database = database
            .lock()
            .map_err(|_| anyhow::anyhow!("database lock poisoned"))?;
        Ok(match request {
            Request::WhoAmI => Response::Identity {
                uid: account.uid,
                handle: account.username.clone(),
                groups: account.groups.clone(),
            },
            Request::ListBoards => Response::Boards(database.boards()?),
            Request::ListPosts {
                board,
                limit,
                offset,
            } => match database.list(&board, limit, offset)? {
                Some(posts) => Response::Posts(posts),
                None => error(ErrorCode::NotFound, "board not found"),
            },
            Request::GetPost { id } => match database.get(id, account.uid)? {
                Some(post) => Response::Post(post),
                None => error(ErrorCode::NotFound, "post not found"),
            },
            Request::CreatePost { board, title, body } => {
                create_post(&mut database, account, &board, &title, &body)?
            }
            Request::CreatePoll {
                board,
                title,
                body,
                options,
            } => create_poll(&mut database, account, &board, &title, &body, &options)?,
            Request::UpdatePost { id, title, body } => {
                update_post(&mut database, account, id, &title, &body)?
            }
            Request::DeletePost { id } => delete_post(&mut database, account, id)?,
            Request::CastVote { post_id, option_id } => {
                cast_vote(&mut database, account, post_id, option_id)?
            }
            Request::CreateReply { post_id, body } => {
                create_reply(&mut database, account, post_id, &body)?
            }
            Request::UpdateReply { id, body } => update_reply(&mut database, account, id, &body)?,
            Request::DeleteReply { id } => delete_reply(&mut database, account, id)?,
            Request::SetPostLocked { id, locked } => {
                set_post_locked(&mut database, account, id, locked)?
            }
        })
    })();

    result.unwrap_or_else(|cause| error(ErrorCode::BadRequest, &cause.to_string()))
}

fn create_post(
    database: &mut Database,
    account: &peer::Account,
    slug: &str,
    title: &str,
    body: &str,
) -> Result<Response> {
    let Some(board) = database.board(slug)? else {
        return Ok(error(ErrorCode::NotFound, "board not found"));
    };
    if board.kind != BoardKind::Discussion {
        return Ok(error(
            ErrorCode::BadRequest,
            "use a poll when posting to this board",
        ));
    }
    if !can_write(&board, account) {
        return Ok(forbidden_for_board(&board));
    }
    Ok(Response::Created(database.create(
        &board,
        account.uid,
        &account.username,
        title,
        body,
    )?))
}

fn create_poll(
    database: &mut Database,
    account: &peer::Account,
    slug: &str,
    title: &str,
    body: &str,
    options: &[String],
) -> Result<Response> {
    let Some(board) = database.board(slug)? else {
        return Ok(error(ErrorCode::NotFound, "board not found"));
    };
    if board.kind != BoardKind::Polls {
        return Ok(error(ErrorCode::BadRequest, "polls are not allowed here"));
    }
    if !can_write(&board, account) {
        return Ok(forbidden_for_board(&board));
    }
    Ok(Response::Created(database.create_poll(
        &board,
        account.uid,
        &account.username,
        title,
        body,
        options,
    )?))
}

fn update_post(
    database: &mut Database,
    account: &peer::Account,
    id: i64,
    title: &str,
    body: &str,
) -> Result<Response> {
    let Some(post) = database.get(id, account.uid)? else {
        return Ok(error(ErrorCode::NotFound, "post not found"));
    };
    if database.owner_uid(id)? != Some(account.uid) {
        return Ok(error(ErrorCode::Forbidden, "not your post"));
    }
    if !can_write(&post.board, account) {
        return Ok(forbidden_for_board(&post.board));
    }
    Ok(match database.update(account.uid, id, title, body)? {
        Some(post) => Response::Updated(post),
        None => error(ErrorCode::NotFound, "post not found"),
    })
}

fn delete_post(database: &mut Database, account: &peer::Account, id: i64) -> Result<Response> {
    let Some(post) = database.get(id, account.uid)? else {
        return Ok(error(ErrorCode::NotFound, "post not found"));
    };
    if database.owner_uid(id)? != Some(account.uid) {
        return Ok(error(ErrorCode::Forbidden, "not your post"));
    }
    if !can_write(&post.board, account) {
        return Ok(forbidden_for_board(&post.board));
    }
    Ok(if database.delete(account.uid, id)? {
        Response::Deleted { id }
    } else {
        error(ErrorCode::NotFound, "post not found")
    })
}

fn cast_vote(
    database: &mut Database,
    account: &peer::Account,
    post_id: i64,
    option_id: i64,
) -> Result<Response> {
    let Some(post) = database.get(post_id, account.uid)? else {
        return Ok(error(ErrorCode::NotFound, "proposal not found"));
    };
    if post.board.kind != BoardKind::Polls {
        return Ok(error(ErrorCode::BadRequest, "post has no poll"));
    }
    if !can_write(&post.board, account) {
        return Ok(forbidden_for_board(&post.board));
    }
    if post.locked {
        return Ok(error(ErrorCode::Forbidden, "proposal is locked"));
    }
    Ok(match database.vote(account.uid, post_id, option_id)? {
        Some(post) => Response::Voted(post),
        None => error(ErrorCode::NotFound, "poll option not found"),
    })
}

fn create_reply(
    database: &mut Database,
    account: &peer::Account,
    post_id: i64,
    body: &str,
) -> Result<Response> {
    let Some(post) = database.get(post_id, account.uid)? else {
        return Ok(error(ErrorCode::NotFound, "post not found"));
    };
    if post.locked {
        return Ok(error(ErrorCode::Forbidden, "post is locked"));
    }
    Ok(
        match database.create_reply(account.uid, &account.username, post_id, body)? {
            Some(post) => Response::Replied(post),
            None => error(ErrorCode::Forbidden, "post is locked"),
        },
    )
}

fn update_reply(
    database: &mut Database,
    account: &peer::Account,
    id: i64,
    body: &str,
) -> Result<Response> {
    let Some(owner_uid) = database.reply_owner_uid(id)? else {
        return Ok(error(ErrorCode::NotFound, "reply not found"));
    };
    if owner_uid != account.uid {
        return Ok(error(ErrorCode::Forbidden, "not your reply"));
    }
    let Some(post_id) = database.update_reply(account.uid, id, body)? else {
        return Ok(error(ErrorCode::NotFound, "reply not found"));
    };
    Ok(match database.get(post_id, account.uid)? {
        Some(post) => Response::ReplyUpdated(post),
        None => error(ErrorCode::NotFound, "post not found"),
    })
}

fn delete_reply(database: &mut Database, account: &peer::Account, id: i64) -> Result<Response> {
    let Some(owner_uid) = database.reply_owner_uid(id)? else {
        return Ok(error(ErrorCode::NotFound, "reply not found"));
    };
    if owner_uid != account.uid {
        return Ok(error(ErrorCode::Forbidden, "not your reply"));
    }
    Ok(match database.delete_reply(account.uid, id)? {
        Some(post_id) => Response::ReplyDeleted { id, post_id },
        None => error(ErrorCode::NotFound, "reply not found"),
    })
}

fn set_post_locked(
    database: &mut Database,
    account: &peer::Account,
    id: i64,
    locked: bool,
) -> Result<Response> {
    if !is_wheel(account) {
        return Ok(error(
            ErrorCode::Forbidden,
            "locking posts requires Unix group wheel",
        ));
    }
    Ok(match database.set_locked(id, locked, account.uid)? {
        Some(post) => Response::LockChanged(post),
        None => error(ErrorCode::NotFound, "post not found"),
    })
}

fn can_write(board: &Board, account: &peer::Account) -> bool {
    board
        .write_group
        .as_ref()
        .is_none_or(|group| account.groups.iter().any(|candidate| candidate == group))
}

fn is_wheel(account: &peer::Account) -> bool {
    account.groups.iter().any(|group| group == "wheel")
}

fn forbidden_for_board(board: &Board) -> Response {
    let message = board.write_group.as_ref().map_or_else(
        || format!("you cannot write to {}", board.name),
        |group| format!("writing to {} requires Unix group {group}", board.name),
    );
    error(ErrorCode::Forbidden, &message)
}

fn error(code: ErrorCode, message: &str) -> Response {
    Response::Error {
        code,
        message: message.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use std::{path::Path, sync::Mutex};

    use salyut_bbs::{
        db::Database,
        peer::Account,
        protocol::{Board, BoardKind, ErrorCode, Post, Request, Response},
    };

    use super::{can_write, dispatch, is_wheel};

    fn updates() -> Board {
        Board {
            id: 2,
            slug: "updates".to_owned(),
            name: "Updates".to_owned(),
            description: String::new(),
            kind: BoardKind::Discussion,
            write_group: Some("wheel".to_owned()),
        }
    }

    #[test]
    fn restricted_board_uses_resolved_unix_groups() {
        let member = Account {
            uid: 1001,
            username: "alice".to_owned(),
            groups: vec!["users".to_owned(), "wheel".to_owned()],
        };
        let non_member = Account {
            uid: 1002,
            username: "bob".to_owned(),
            groups: vec!["users".to_owned()],
        };
        assert!(can_write(&updates(), &member));
        assert!(!can_write(&updates(), &non_member));
        assert!(is_wheel(&member));
        assert!(!is_wheel(&non_member));
    }

    #[test]
    fn only_wheel_can_lock_a_post() {
        let database = Mutex::new(Database::open(Path::new(":memory:")).unwrap());
        let post = {
            let mut database = database.lock().unwrap();
            let board = database.board("general").unwrap().unwrap();
            database
                .create(&board, 1001, "alice", "Thread", "Body")
                .unwrap()
        };
        let member = Account {
            uid: 1001,
            username: "alice".to_owned(),
            groups: vec!["users".to_owned(), "wheel".to_owned()],
        };
        let non_member = Account {
            uid: 1002,
            username: "bob".to_owned(),
            groups: vec!["users".to_owned()],
        };

        assert!(matches!(
            dispatch(
                &database,
                &non_member,
                Request::SetPostLocked {
                    id: post.id,
                    locked: true
                }
            ),
            Response::Error {
                code: ErrorCode::Forbidden,
                ..
            }
        ));
        assert!(matches!(
            dispatch(
                &database,
                &member,
                Request::SetPostLocked {
                    id: post.id,
                    locked: true
                }
            ),
            Response::LockChanged(Post { locked: true, .. })
        ));
    }

    #[test]
    fn updates_allow_replies_from_non_wheel_users() {
        let database = Mutex::new(Database::open(Path::new(":memory:")).unwrap());
        let post = {
            let mut database = database.lock().unwrap();
            let board = database.board("updates").unwrap().unwrap();
            database
                .create(&board, 1001, "alice", "Maintenance", "Tonight at 20:00.")
                .unwrap()
        };
        let account = Account {
            uid: 1002,
            username: "bob".to_owned(),
            groups: vec!["users".to_owned()],
        };

        assert!(matches!(
            dispatch(
                &database,
                &account,
                Request::CreatePost {
                    board: "updates".to_owned(),
                    title: "Not allowed".to_owned(),
                    body: "Top-level post".to_owned(),
                }
            ),
            Response::Error {
                code: ErrorCode::Forbidden,
                ..
            }
        ));
        assert!(matches!(
            dispatch(
                &database,
                &account,
                Request::CreateReply {
                    post_id: post.id,
                    body: "Thanks for the warning.".to_owned(),
                }
            ),
            Response::Replied(Post { replies, .. }) if replies.len() == 1
        ));
    }
}
