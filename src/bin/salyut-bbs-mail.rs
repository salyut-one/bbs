use std::{
    io::{self, Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use mailparse::{DispositionType, MailHeaderMap, ParsedMail};
use salyut_bbs::{client::Client, protocol::MailDelivery};

const MAX_MESSAGE_BYTES: u64 = 512 * 1024;
const REPLY_MARKER: &str = "--- reply above this line ---";
const MAIL_DOMAIN: &str = "bbs.salyut.one";

#[derive(Parser)]
#[command(version, about = "Postfix bridge for the salyut.one BBS")]
struct Arguments {
    #[arg(long, default_value_os_t = salyut_bbs::paths::read_write_socket())]
    socket: PathBuf,
    #[command(subcommand)]
    command: MailCommand,
}

#[derive(Subcommand)]
enum MailCommand {
    Deliver {
        #[arg(long, default_value = "/usr/sbin/sendmail")]
        sendmail: PathBuf,
        #[arg(long)]
        once: bool,
    },
    Receive {
        #[arg(long)]
        recipient: String,
        #[arg(long, default_value = "")]
        sasl_username: String,
    },
}

fn main() -> Result<()> {
    let arguments = Arguments::parse();
    let client = Client::new(arguments.socket);
    match arguments.command {
        MailCommand::Deliver { sendmail, once } => deliver(&client, &sendmail, once),
        MailCommand::Receive {
            recipient,
            sasl_username,
        } => receive(&client, &recipient, &sasl_username, io::stdin()),
    }
}

fn deliver(client: &Client, sendmail: &Path, once: bool) -> Result<()> {
    loop {
        let Some(delivery) = client.claim_mail_delivery()? else {
            if once {
                return Ok(());
            }
            thread::sleep(Duration::from_secs(2));
            continue;
        };
        match send_delivery(sendmail, &delivery) {
            Ok(()) => client.complete_mail_delivery(delivery.id)?,
            Err(error) => {
                let message = format!("{error:#}");
                client.fail_mail_delivery(delivery.id, &message)?;
                if once {
                    return Err(error);
                }
                eprintln!("delivery {} failed: {message}", delivery.id);
            }
        }
        if once {
            return Ok(());
        }
    }
}

fn send_delivery(sendmail: &Path, delivery: &MailDelivery) -> Result<()> {
    validate_local_username(&delivery.recipient)?;
    let mut child = Command::new(sendmail)
        .args(["-i", "--"])
        .arg(&delivery.recipient)
        .stdin(Stdio::piped())
        .spawn()
        .with_context(|| format!("start {}", sendmail.display()))?;
    child
        .stdin
        .take()
        .context("sendmail stdin is unavailable")?
        .write_all(render_message(delivery)?.as_bytes())?;
    let status = child.wait().context("wait for sendmail")?;
    if !status.success() {
        bail!("sendmail exited with {status}");
    }
    Ok(())
}

fn render_message(delivery: &MailDelivery) -> Result<String> {
    validate_local_username(&delivery.recipient)?;
    validate_token(&delivery.reply_token)?;
    validate_token(&delivery.unsubscribe_token)?;
    let author = sanitize_header(&delivery.author);
    let subject = sanitize_header(&delivery.subject);
    let board = sanitize_header(&delivery.board_slug);
    let mut headers = vec![
        format!("From: {author} via Salyut BBS <bbs@salyut.one>"),
        format!("To: {}@salyut.one", delivery.recipient),
        format!("Subject: [salyut-bbs/{board}] {subject}"),
        format!("Date: {}", chrono::Utc::now().to_rfc2822()),
        format!("Message-ID: {}", delivery.message_id),
        format!("Reply-To: reply+{}@{MAIL_DOMAIN}", delivery.reply_token),
        format!("List-Id: {board} <{board}.{MAIL_DOMAIN}>"),
        format!(
            "List-Unsubscribe: <mailto:unsubscribe+{}@{MAIL_DOMAIN}>",
            delivery.unsubscribe_token
        ),
        "MIME-Version: 1.0".to_owned(),
        "Content-Type: text/plain; charset=UTF-8".to_owned(),
        "Content-Transfer-Encoding: 8bit".to_owned(),
        "Auto-Submitted: no".to_owned(),
        format!("X-Salyut-BBS-Post: {}", delivery.post_id),
    ];
    if let Some(in_reply_to) = &delivery.in_reply_to {
        headers.push(format!("In-Reply-To: {in_reply_to}"));
        headers.push(format!("References: {in_reply_to}"));
    }
    let body = delivery.body.replace("\r\n", "\n").replace('\r', "\n");
    Ok(format!(
        "{}\r\n\r\n{REPLY_MARKER}\r\n\
         Reply to this message to post a reply as your Unix account.\r\n\r\n\
         @{author} wrote:\r\n\r\n{}\r\n\r\n\
         View: https://salyut.one/bbs/posts/{}\r\n\
         Unsubscribe from /{board}: mailto:unsubscribe+{}@{MAIL_DOMAIN}\r\n",
        headers.join("\r\n"),
        body.replace('\n', "\r\n"),
        delivery.post_id,
        delivery.unsubscribe_token,
    ))
}

fn receive(client: &Client, recipient: &str, sasl_username: &str, input: impl Read) -> Result<()> {
    let route = parse_recipient(recipient)?;
    let mut raw = Vec::new();
    input
        .take(MAX_MESSAGE_BYTES + 1)
        .read_to_end(&mut raw)
        .context("read message from Postfix")?;
    if raw.len() as u64 > MAX_MESSAGE_BYTES {
        bail!("message exceeds {MAX_MESSAGE_BYTES} bytes");
    }
    match route {
        Route::Post(board) => {
            let username = authenticated_username(sasl_username)?;
            let post = parse_mail_post(&raw)?;
            let (post_id, duplicate) = client.import_mail_post(
                board,
                username,
                &post.message_id,
                &post.title,
                &post.body,
            )?;
            if duplicate {
                eprintln!("ignored duplicate mail post #{post_id}");
            } else {
                eprintln!("created post #{post_id} in /{board} from authenticated mail");
            }
        }
        Route::Unsubscribe(token) => {
            let board = client.unsubscribe_mail_token(token)?;
            eprintln!("unsubscribed mail recipient from /{board}");
        }
        Route::Reply(token) => {
            let parsed = mailparse::parse_mail(&raw).context("parse MIME message")?;
            reject_automatic_message(&parsed)?;
            let message_id = parsed
                .headers
                .get_first_value("Message-ID")
                .context("message has no Message-ID")?;
            let body = plain_text_body(&parsed)?.context("message has no text/plain body")?;
            let body = reply_text(&body)?;
            let (post_id, duplicate) = client.import_mail_reply(token, &message_id, &body)?;
            if duplicate {
                eprintln!("ignored duplicate mail reply to post #{post_id}");
            }
        }
    }
    Ok(())
}

enum Route<'a> {
    Post(&'a str),
    Reply(&'a str),
    Unsubscribe(&'a str),
}

fn parse_recipient(recipient: &str) -> Result<Route<'_>> {
    let (local, domain) = recipient
        .rsplit_once('@')
        .context("recipient is not an email address")?;
    if !domain.eq_ignore_ascii_case(MAIL_DOMAIN) {
        bail!("recipient is outside {MAIL_DOMAIN}");
    }
    let Some((kind, token)) = local.split_once('+') else {
        validate_board_slug(local)?;
        return Ok(Route::Post(local));
    };
    validate_token(token)?;
    match kind {
        "reply" => Ok(Route::Reply(token)),
        "unsubscribe" => Ok(Route::Unsubscribe(token)),
        _ => bail!("unknown BBS mail route"),
    }
}

struct MailPost {
    message_id: String,
    title: String,
    body: String,
}

fn parse_mail_post(raw: &[u8]) -> Result<MailPost> {
    let parsed = mailparse::parse_mail(raw).context("parse MIME message")?;
    reject_automatic_message(&parsed)?;
    let message_id = parsed
        .headers
        .get_first_value("Message-ID")
        .context("message has no Message-ID")?;
    let title = parsed
        .headers
        .get_first_value("Subject")
        .context("message has no Subject")?
        .trim()
        .to_owned();
    if title.is_empty() {
        bail!("message Subject is empty");
    }
    let body = plain_text_body(&parsed)?.context("message has no text/plain body")?;
    let body = body.replace("\r\n", "\n").replace('\r', "\n");
    let body = body.trim().to_owned();
    if body.is_empty() {
        bail!("mail post body is empty");
    }
    Ok(MailPost {
        message_id,
        title,
        body,
    })
}

fn authenticated_username(username: &str) -> Result<&str> {
    if username.is_empty() {
        bail!("posting by email requires authenticated SMTP submission");
    }
    validate_local_username(username)?;
    Ok(username)
}

fn reject_automatic_message(message: &ParsedMail<'_>) -> Result<()> {
    if let Some(value) = message.headers.get_first_value("Auto-Submitted")
        && !value.eq_ignore_ascii_case("no")
    {
        bail!("automatically generated messages are not accepted");
    }
    Ok(())
}

fn plain_text_body(message: &ParsedMail<'_>) -> Result<Option<String>> {
    if message.ctype.mimetype.eq_ignore_ascii_case("text/plain")
        && message.get_content_disposition().disposition != DispositionType::Attachment
    {
        return message
            .get_body()
            .map(Some)
            .context("decode text/plain message body");
    }
    for part in &message.subparts {
        if let Some(body) = plain_text_body(part)? {
            return Ok(Some(body));
        }
    }
    Ok(None)
}

fn reply_text(body: &str) -> Result<String> {
    let body = body.replace("\r\n", "\n").replace('\r', "\n");
    let mut kept = Vec::new();
    for line in body.lines() {
        let possible_marker = line.trim_start().trim_start_matches('>').trim_start();
        if possible_marker == REPLY_MARKER {
            break;
        }
        kept.push(line);
    }
    let reply = kept.join("\n").trim().to_owned();
    if reply.is_empty() {
        bail!("mail reply body is empty");
    }
    Ok(reply)
}

fn validate_token(token: &str) -> Result<()> {
    if token.len() != 64
        || !token
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        bail!("invalid route token");
    }
    Ok(())
}

fn validate_board_slug(slug: &str) -> Result<()> {
    if slug.is_empty()
        || slug.len() > 64
        || !slug
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        bail!("invalid board address");
    }
    Ok(())
}

fn validate_local_username(username: &str) -> Result<()> {
    if username.is_empty()
        || !username
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        bail!("invalid local recipient username");
    }
    Ok(())
}

fn sanitize_header(value: &str) -> String {
    value
        .chars()
        .map(|character| match character {
            '\r' | '\n' | '\0' => ' ',
            character => character,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{
        MAIL_DOMAIN, REPLY_MARKER, Route, authenticated_username, parse_mail_post, parse_recipient,
        render_message, reply_text,
    };
    use salyut_bbs::protocol::MailDelivery;

    fn delivery() -> MailDelivery {
        MailDelivery {
            id: 1,
            recipient: "alice".to_owned(),
            board_slug: "general".to_owned(),
            post_id: 42,
            author: "bob".to_owned(),
            subject: "Hello".to_owned(),
            body: "Opening post.".to_owned(),
            message_id: "<bbs-post-42@salyut.one>".to_owned(),
            in_reply_to: None,
            reply_token: "a".repeat(64),
            unsubscribe_token: "b".repeat(64),
        }
    }

    #[test]
    fn message_contains_thread_and_unsubscribe_routes() {
        let message = render_message(&delivery()).unwrap();
        assert!(message.contains(&format!("Reply-To: reply+{}@{MAIL_DOMAIN}", "a".repeat(64))));
        assert!(message.contains(&format!(
            "List-Unsubscribe: <mailto:unsubscribe+{}@{MAIL_DOMAIN}>",
            "b".repeat(64)
        )));
        assert!(message.contains(REPLY_MARKER));
    }

    #[test]
    fn quoted_original_is_removed_from_reply() {
        let body = format!("My answer.\n\n> {REPLY_MARKER}\n> Original");
        assert_eq!(reply_text(&body).unwrap(), "My answer.");
    }

    #[test]
    fn recipient_route_is_strict() {
        let address = format!("reply+{}@{MAIL_DOMAIN}", "c".repeat(64));
        assert!(matches!(
            parse_recipient(&address).unwrap(),
            Route::Reply(_)
        ));
        assert!(matches!(
            parse_recipient("updates@bbs.salyut.one").unwrap(),
            Route::Post("updates")
        ));
        assert!(parse_recipient("Updates@bbs.salyut.one").is_err());
        assert!(parse_recipient("updates+invalid@bbs.salyut.one").is_err());
        assert!(parse_recipient(&format!("reply+{}@example.com", "c".repeat(64))).is_err());
    }

    #[test]
    fn new_post_requires_authenticated_smtp_and_plain_text() {
        assert!(authenticated_username("").is_err());
        assert_eq!(authenticated_username("alice").unwrap(), "alice");
        let message = b"From: Alice <alice@salyut.one>\r\n\
            To: updates@bbs.salyut.one\r\n\
            Subject: Planned maintenance\r\n\
            Message-ID: <maintenance-1@salyut.one>\r\n\
            MIME-Version: 1.0\r\n\
            Content-Type: text/plain; charset=UTF-8\r\n\
            \r\n\
            Services will restart tonight.\r\n";
        let post = parse_mail_post(message).unwrap();
        assert_eq!(post.message_id, "<maintenance-1@salyut.one>");
        assert_eq!(post.title, "Planned maintenance");
        assert_eq!(post.body, "Services will restart tonight.");
    }

    #[test]
    fn automatic_mail_cannot_create_a_post() {
        let message = b"Subject: Automatic\r\n\
            Message-ID: <automatic@salyut.one>\r\n\
            Auto-Submitted: auto-generated\r\n\
            Content-Type: text/plain\r\n\
            \r\n\
            Do not post this.\r\n";
        assert!(parse_mail_post(message).is_err());
    }
}
