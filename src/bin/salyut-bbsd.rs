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
    protocol::{Board, BoardKind, ErrorCode, ProposalState, Request, Response},
};

#[derive(Parser)]
#[command(version, about = "salyut.one BBS daemon")]
struct Arguments {
    #[arg(long, default_value_os_t = salyut_bbs::paths::read_write_socket())]
    socket: PathBuf,
    #[arg(long, default_value_os_t = salyut_bbs::paths::database())]
    database: PathBuf,
    #[arg(long, default_value = "0660", value_parser = parse_mode)]
    socket_mode: u32,
    #[arg(long, default_value = "salyut-web")]
    read_only_user: String,
    #[arg(long, default_value = "salyut-bbs-mail")]
    mail_user: String,
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
    if let Some(parent) = arguments.database.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create database directory {}", parent.display()))?;
    }

    let database = Arc::new(Mutex::new(Database::open(&arguments.database)?));
    let listener = bind_socket(&arguments.socket, arguments.socket_mode)?;
    eprintln!("salyut-bbsd listening on {}", arguments.socket.display());

    accept_connections(
        listener,
        database,
        Arc::from(arguments.read_only_user),
        Arc::from(arguments.mail_user),
    );
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

fn accept_connections(
    listener: UnixListener,
    database: Arc<Mutex<Database>>,
    read_only_user: Arc<str>,
    mail_user: Arc<str>,
) {
    for connection in listener.incoming() {
        match connection {
            Ok(stream) => {
                let database = Arc::clone(&database);
                let read_only_user = Arc::clone(&read_only_user);
                let mail_user = Arc::clone(&mail_user);
                thread::spawn(move || {
                    if let Err(error) = serve_client(stream, database, &read_only_user, &mail_user)
                    {
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
    read_only_user: &str,
    mail_user: &str,
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
            Ok(request) if denied_for_mail_bridge(&account, mail_user, &request) => error(
                ErrorCode::Forbidden,
                "operation is restricted to its BBS client",
            ),
            Ok(request) if denied_for_read_only_user(&account, read_only_user, &request) => error(
                ErrorCode::Forbidden,
                "this account has read-only BBS access",
            ),
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

fn denied_for_read_only_user(
    account: &peer::Account,
    read_only_user: &str,
    request: &Request,
) -> bool {
    account.username == read_only_user && request.is_mutating()
}

fn denied_for_mail_bridge(account: &peer::Account, mail_user: &str, request: &Request) -> bool {
    request.is_mail_worker() != (account.username == mail_user)
}

fn dispatch(database: &Mutex<Database>, account: &peer::Account, request: Request) -> Response {
    let result = (|| -> Result<Response> {
        let mut database = database
            .lock()
            .map_err(|_| anyhow::anyhow!("database lock poisoned"))?;
        database.finalize_due_proposals(chrono::Utc::now())?;
        Ok(match request {
            Request::WhoAmI => Response::Identity {
                handle: account.username.clone(),
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
                Some(post) => Response::Post(Box::new(post)),
                None => error(ErrorCode::NotFound, "post not found"),
            },
            Request::CreatePost { board, title, body } => {
                create_post(&mut database, account, &board, &title, &body, false)?
            }
            Request::CreateProposal { board, title, body } => {
                create_post(&mut database, account, &board, &title, &body, true)?
            }
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
            Request::WithdrawProposal { id } => withdraw_proposal(&mut database, account, id)?,
            Request::VetoProposal { id, reason } => {
                veto_proposal(&mut database, account, id, &reason)?
            }
            Request::MarkProposalImplemented { id, note } => {
                mark_proposal_implemented(&mut database, account, id, &note)?
            }
            Request::GetMailSubscription { board } => {
                get_mail_subscription(&database, account, &board)?
            }
            Request::SetMailSubscription { board, subscribed } => {
                set_mail_subscription(&mut database, account, &board, subscribed)?
            }
            Request::MailClaimDelivery => Response::MailDelivery(
                database
                    .claim_mail_delivery(chrono::Utc::now())?
                    .map(Box::new),
            ),
            Request::MailCompleteDelivery { id } => {
                if database.complete_mail_delivery(id)? {
                    Response::MailDeliveryUpdated { id }
                } else {
                    error(ErrorCode::NotFound, "mail delivery is not leased")
                }
            }
            Request::MailFailDelivery { id, error: message } => {
                if database.fail_mail_delivery(id, &message)? {
                    Response::MailDeliveryUpdated { id }
                } else {
                    error(ErrorCode::NotFound, "mail delivery is not leased")
                }
            }
            Request::MailImportReply {
                token,
                message_id,
                body,
            } => import_mail_reply(&mut database, &token, &message_id, &body)?,
            Request::MailUnsubscribe { token } => match database.unsubscribe_mail_token(&token)? {
                Some(board) => Response::MailUnsubscribed { board },
                None => error(ErrorCode::NotFound, "unsubscribe token not found"),
            },
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
    proposal: bool,
) -> Result<Response> {
    let Some(board) = database.board(slug)? else {
        return Ok(error(ErrorCode::NotFound, "board not found"));
    };
    let expected = if proposal {
        BoardKind::Polls
    } else {
        BoardKind::Discussion
    };
    if board.kind != expected {
        let message = if proposal {
            "proposals are not allowed here"
        } else {
            "use a proposal when posting to this board"
        };
        return Ok(error(ErrorCode::BadRequest, message));
    }
    if !can_write(&board, account) {
        return Ok(forbidden_for_board(&board));
    }
    let recipients = peer::mail_recipients()?;
    let post = if proposal {
        database.create_proposal_with_mail(
            &board,
            account.uid,
            &account.username,
            title,
            body,
            &recipients,
        )?
    } else {
        database.create_with_mail(
            &board,
            account.uid,
            &account.username,
            title,
            body,
            &recipients,
        )?
    };
    Ok(Response::Post(Box::new(post)))
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
    if post.proposal.is_some() {
        return Ok(error(
            ErrorCode::Forbidden,
            "proposals cannot be edited after voting begins",
        ));
    }
    if !can_write(&post.board, account) {
        return Ok(forbidden_for_board(&post.board));
    }
    Ok(post_or(
        database.update(account.uid, id, title, body)?,
        ErrorCode::NotFound,
        "post not found",
    ))
}

fn delete_post(database: &mut Database, account: &peer::Account, id: i64) -> Result<Response> {
    let Some(post) = database.get(id, account.uid)? else {
        return Ok(error(ErrorCode::NotFound, "post not found"));
    };
    if database.owner_uid(id)? != Some(account.uid) {
        return Ok(error(ErrorCode::Forbidden, "not your post"));
    }
    if post.proposal.is_some() {
        return Ok(error(
            ErrorCode::Forbidden,
            "withdraw proposals instead of deleting them",
        ));
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
    if post.proposal.as_ref().map(|proposal| proposal.state) != Some(ProposalState::Voting) {
        return Ok(error(ErrorCode::Forbidden, "voting is closed"));
    }
    Ok(post_or(
        database.vote(account.uid, post_id, option_id)?,
        ErrorCode::NotFound,
        "poll option not found",
    ))
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
    let recipients = peer::mail_recipients()?;
    Ok(post_or(
        database.create_reply_with_mail(
            account.uid,
            &account.username,
            post_id,
            body,
            &recipients,
        )?,
        ErrorCode::Forbidden,
        "post is locked",
    ))
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
    Ok(post_or(
        database.get(post_id, account.uid)?,
        ErrorCode::NotFound,
        "post not found",
    ))
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
    Ok(post_or(
        database.set_locked(id, locked, account.uid)?,
        ErrorCode::NotFound,
        "post not found",
    ))
}

fn withdraw_proposal(
    database: &mut Database,
    account: &peer::Account,
    id: i64,
) -> Result<Response> {
    let Some(post) = database.get(id, account.uid)? else {
        return Ok(error(ErrorCode::NotFound, "proposal not found"));
    };
    if database.owner_uid(id)? != Some(account.uid) {
        return Ok(error(
            ErrorCode::Forbidden,
            "only the proposal author may withdraw it",
        ));
    }
    if post.proposal.as_ref().map(|proposal| proposal.state) != Some(ProposalState::Voting) {
        return Ok(error(
            ErrorCode::Forbidden,
            "only a proposal with open voting may be withdrawn",
        ));
    }
    Ok(post_or(
        database.withdraw_proposal(id, account.uid, &account.username)?,
        ErrorCode::Forbidden,
        "proposal state changed",
    ))
}

fn veto_proposal(
    database: &mut Database,
    account: &peer::Account,
    id: i64,
    reason: &str,
) -> Result<Response> {
    if !is_wheel(account) {
        return Ok(error(
            ErrorCode::Forbidden,
            "vetoing proposals requires Unix group wheel",
        ));
    }
    let Some(post) = database.get(id, account.uid)? else {
        return Ok(error(ErrorCode::NotFound, "proposal not found"));
    };
    if post.proposal.as_ref().map(|proposal| proposal.state) != Some(ProposalState::Accepted) {
        return Ok(error(
            ErrorCode::Forbidden,
            "only an accepted proposal may be vetoed",
        ));
    }
    Ok(post_or(
        database.veto_proposal(id, account.uid, &account.username, reason)?,
        ErrorCode::Forbidden,
        "proposal state changed",
    ))
}

fn mark_proposal_implemented(
    database: &mut Database,
    account: &peer::Account,
    id: i64,
    note: &str,
) -> Result<Response> {
    if !is_wheel(account) {
        return Ok(error(
            ErrorCode::Forbidden,
            "implementing proposals requires Unix group wheel",
        ));
    }
    let Some(post) = database.get(id, account.uid)? else {
        return Ok(error(ErrorCode::NotFound, "proposal not found"));
    };
    if post.proposal.as_ref().map(|proposal| proposal.state) != Some(ProposalState::Accepted) {
        return Ok(error(
            ErrorCode::Forbidden,
            "only an accepted proposal may be marked implemented",
        ));
    }
    Ok(post_or(
        database.mark_proposal_implemented(id, account.uid, &account.username, note)?,
        ErrorCode::Forbidden,
        "proposal state changed",
    ))
}

fn get_mail_subscription(
    database: &Database,
    account: &peer::Account,
    board: &str,
) -> Result<Response> {
    let eligible = peer::mail_eligible(account.uid);
    Ok(
        match database.mail_subscription(account.uid, board, eligible)? {
            Some(subscribed) => Response::MailSubscription {
                board: board.to_owned(),
                subscribed,
                eligible,
            },
            None => error(ErrorCode::NotFound, "board not found"),
        },
    )
}

fn set_mail_subscription(
    database: &mut Database,
    account: &peer::Account,
    board: &str,
    subscribed: bool,
) -> Result<Response> {
    if !peer::mail_eligible(account.uid) {
        return Ok(error(
            ErrorCode::Forbidden,
            "mail delivery requires a Unix UID from 1000 through 59999",
        ));
    }
    Ok(
        match database.set_mail_subscription(account.uid, &account.username, board, subscribed)? {
            Some(subscribed) => Response::MailSubscription {
                board: board.to_owned(),
                subscribed,
                eligible: true,
            },
            None => error(ErrorCode::NotFound, "board not found"),
        },
    )
}

fn import_mail_reply(
    database: &mut Database,
    token: &str,
    message_id: &str,
    body: &str,
) -> Result<Response> {
    if let Some(post_id) = database.imported_mail_post(message_id)? {
        return Ok(Response::MailReplyAccepted {
            post_id,
            duplicate: true,
        });
    }
    let Some((post_id, uid)) = database.mail_reply_target(token)? else {
        return Ok(error(ErrorCode::NotFound, "mail reply token not found"));
    };
    if !peer::mail_eligible(uid) {
        return Ok(error(
            ErrorCode::Forbidden,
            "mail reply account is no longer eligible",
        ));
    }
    let account = peer::account(uid)?;
    let recipients = peer::mail_recipients()?;
    Ok(
        match database.import_mail_reply(
            account.uid,
            &account.username,
            post_id,
            message_id,
            body,
            &recipients,
        )? {
            Some(imported) => Response::MailReplyAccepted {
                post_id: imported.post_id,
                duplicate: imported.duplicate,
            },
            None => error(ErrorCode::Forbidden, "post is locked or no longer exists"),
        },
    )
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

fn post_or(post: Option<salyut_bbs::protocol::Post>, code: ErrorCode, message: &str) -> Response {
    post.map_or_else(
        || error(code, message),
        |post| Response::Post(Box::new(post)),
    )
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
        protocol::{ErrorCode, Post, ProposalState, Request, Response},
    };

    use super::{denied_for_mail_bridge, denied_for_read_only_user, dispatch};

    fn account(uid: u32, name: &str, wheel: bool) -> Account {
        Account {
            uid,
            username: name.to_owned(),
            groups: wheel.then(|| "wheel".to_owned()).into_iter().collect(),
        }
    }

    fn database() -> Mutex<Database> {
        Mutex::new(Database::open(Path::new(":memory:")).unwrap())
    }

    fn create(database: &Mutex<Database>, board: &str) -> Post {
        let mut database = database.lock().unwrap();
        let board = database.board(board).unwrap().unwrap();
        database
            .create(&board, 1001, "alice", "Thread", "Body")
            .unwrap()
    }

    fn proposal(database: &Mutex<Database>) -> Post {
        let mut database = database.lock().unwrap();
        let board = database.board("proposals").unwrap().unwrap();
        database
            .create_proposal(&board, 1001, "alice", "Garden?", "Plant herbs.")
            .unwrap()
    }

    fn forbidden(response: Response) -> bool {
        matches!(
            response,
            Response::Error {
                code: ErrorCode::Forbidden,
                ..
            }
        )
    }

    #[test]
    fn restricted_board_uses_resolved_unix_groups() {
        let database = database();
        let request = || Request::CreatePost {
            board: "updates".to_owned(),
            title: "Maintenance".to_owned(),
            body: "Tonight.".to_owned(),
        };
        assert!(forbidden(dispatch(
            &database,
            &account(1002, "bob", false),
            request()
        )));
        assert!(matches!(
            dispatch(&database, &account(1001, "alice", true), request()),
            Response::Post(_)
        ));
    }

    #[test]
    fn web_identity_is_read_only_on_the_shared_socket() {
        let web = account(998, "salyut-web", false);
        let write = Request::CreatePost {
            board: "general".to_owned(),
            title: "Hello".to_owned(),
            body: "World".to_owned(),
        };

        assert!(!denied_for_read_only_user(
            &web,
            "salyut-web",
            &Request::ListBoards
        ));
        assert!(denied_for_read_only_user(&web, "salyut-web", &write));
        assert!(!denied_for_read_only_user(
            &account(1001, "alice", false),
            "salyut-web",
            &write
        ));
    }

    #[test]
    fn mail_protocol_is_available_only_to_the_mail_bridge() {
        let user = account(1001, "alice", false);
        let mail = account(997, "salyut-bbs-mail", false);
        assert!(denied_for_mail_bridge(
            &user,
            "salyut-bbs-mail",
            &Request::MailClaimDelivery
        ));
        assert!(!denied_for_mail_bridge(
            &mail,
            "salyut-bbs-mail",
            &Request::MailClaimDelivery
        ));
        assert!(denied_for_mail_bridge(
            &mail,
            "salyut-bbs-mail",
            &Request::ListBoards
        ));
        assert!(!denied_for_mail_bridge(
            &user,
            "salyut-bbs-mail",
            &Request::ListBoards
        ));
    }

    #[test]
    fn only_wheel_can_lock_a_post() {
        let database = database();
        let post = create(&database, "general");
        let request = || Request::SetPostLocked {
            id: post.id,
            locked: true,
        };

        assert!(forbidden(dispatch(
            &database,
            &account(1002, "bob", false),
            request()
        )));
        assert!(matches!(
            dispatch(&database, &account(1001, "alice", true), request()),
            Response::Post(post) if post.locked
        ));
    }

    #[test]
    fn updates_allow_replies_from_non_wheel_users() {
        let database = database();
        let post = create(&database, "updates");
        assert!(matches!(
            dispatch(
                &database,
                &account(1002, "bob", false),
                Request::CreateReply {
                    post_id: post.id,
                    body: "Thanks.".to_owned(),
                }
            ),
            Response::Post(post) if post.replies.len() == 1
        ));
    }

    #[test]
    fn proposals_are_open_to_non_wheel_users() {
        let database = database();

        assert!(matches!(
            dispatch(
                &database,
                &account(1002, "bob", false),
                Request::CreateProposal {
                    board: "proposals".to_owned(),
                    title: "Garden?".to_owned(),
                    body: "Plant herbs.".to_owned(),
                }
            ),
            Response::Post(post) if post.author == "bob" && post.poll.is_some()
        ));
    }

    #[test]
    fn only_author_can_withdraw_an_open_proposal() {
        let database = database();
        let proposal = proposal(&database);
        let request = || Request::WithdrawProposal { id: proposal.id };

        assert!(forbidden(dispatch(
            &database,
            &account(1002, "bob", false),
            request()
        )));
        assert!(matches!(
            dispatch(&database, &account(1001, "alice", false), request()),
            Response::Post(post)
                if post.proposal.as_ref().unwrap().state == ProposalState::Withdrawn
        ));
    }

    #[test]
    fn veto_requires_wheel_and_a_published_reason() {
        let database = database();
        let proposal = proposal(&database);
        {
            let mut database = database.lock().unwrap();
            let option = proposal.poll.as_ref().unwrap().options[0].id;
            database.vote(1001, proposal.id, option).unwrap();
            database
                .finalize_due_proposals(proposal.proposal.as_ref().unwrap().closes_at)
                .unwrap();
        }
        let request = |reason: &str| Request::VetoProposal {
            id: proposal.id,
            reason: reason.to_owned(),
        };

        assert!(forbidden(dispatch(
            &database,
            &account(1002, "bob", false),
            request("No space.")
        )));
        assert!(matches!(
            dispatch(&database, &account(0, "root", true), request("")),
            Response::Error {
                code: ErrorCode::BadRequest,
                ..
            }
        ));
        assert!(matches!(
            dispatch(
                &database,
                &account(0, "root", true),
                request("Exceeds available space.")
            ),
            Response::Post(post)
                if post.proposal.as_ref().unwrap().state == ProposalState::Vetoed
        ));
    }
}
