use std::{
    io::{self, BufRead, BufReader, Read, Write},
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use mailparse::{DispositionType, MailAddr, MailHeaderMap, ParsedMail};
use salyut_bbs::{client::Client, protocol::MailDelivery};

const MAX_MESSAGE_BYTES: u64 = 512 * 1024;
const MAX_LOOKUP_RESPONSE_BYTES: u64 = 512;
const REPLY_MARKER: &str = "--- reply above this line ---";
const MAIL_DOMAIN: &str = "bbs.salyut.one";
const AUTHENTICATION_SERVICE: &str = "mail.salyut.one";

#[derive(Parser)]
#[command(version, about = "Postfix bridge for the salyut.one BBS")]
struct Arguments {
    #[arg(long, default_value_os_t = salyut_bbs::paths::read_write_socket())]
    socket: PathBuf,
    #[arg(
        long,
        default_value = "/run/salyut-bbs/users/forward-map.sock",
        global = true
    )]
    forward_map_socket: PathBuf,
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
        } => receive(
            &client,
            &arguments.forward_map_socket,
            &recipient,
            &sasl_username,
            io::stdin(),
        ),
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

fn receive(
    client: &Client,
    forward_map_socket: &Path,
    recipient: &str,
    sasl_username: &str,
    input: impl Read,
) -> Result<()> {
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
            let post = parse_mail_post(&raw)?;
            let username = posting_username(sasl_username, &raw, forward_map_socket)?;
            let (post_id, duplicate) = client.import_mail_post(
                board,
                &username,
                &post.message_id,
                &post.title,
                &post.body,
            )?;
            if duplicate {
                eprintln!("ignored duplicate mail post #{post_id}");
            } else {
                eprintln!("created post #{post_id} in /{board} from verified mail");
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

fn posting_username(sasl_username: &str, raw: &[u8], forward_map_socket: &Path) -> Result<String> {
    if !sasl_username.is_empty() {
        return authenticated_username(sasl_username).map(str::to_owned);
    }
    let message = mailparse::parse_mail(raw).context("parse MIME message")?;
    let address = verified_external_sender(&message)?;
    lookup_forwarding_user(forward_map_socket, &address)
}

fn verified_external_sender(message: &ParsedMail<'_>) -> Result<String> {
    let from_headers: Vec<_> = message
        .headers
        .iter()
        .filter(|header| header.get_key_ref().eq_ignore_ascii_case("From"))
        .collect();
    if from_headers.len() != 1 {
        bail!("external mail must contain exactly one From header");
    }
    let addresses =
        mailparse::addrparse_header(from_headers[0]).context("parse external From header")?;
    if addresses.len() != 1 {
        bail!("external mail From must contain exactly one mailbox");
    }
    let MailAddr::Single(sender) = &addresses[0] else {
        bail!("external mail From must not be a group");
    };
    let (_, domain) = sender
        .addr
        .rsplit_once('@')
        .context("external From address has no domain")?;
    let domain = domain.trim_end_matches('.').to_ascii_lowercase();
    let authenticated = message.headers.iter().any(|header| {
        header
            .get_key_ref()
            .eq_ignore_ascii_case("Authentication-Results")
            && authentication_result_passes(&header.get_value(), &domain)
    });
    if !authenticated {
        bail!("external mail requires an aligned DKIM pass from {AUTHENTICATION_SERVICE}");
    }
    salyut_bbs::forward_map::normalize_address(&sender.addr)
}

fn authentication_result_passes(value: &str, sender_domain: &str) -> bool {
    let mut parts = value.split(';');
    if !parts
        .next()
        .is_some_and(|part| part.trim().eq_ignore_ascii_case(AUTHENTICATION_SERVICE))
    {
        return false;
    }
    parts.any(|result| {
        let result = remove_comments(result);
        let mut passed = false;
        let mut signing_domain = None;
        for token in result.split_ascii_whitespace() {
            let token = token.trim_matches('"');
            if token.eq_ignore_ascii_case("dkim=pass") {
                passed = true;
            } else if let Some(value) = token
                .strip_prefix("header.d=")
                .or_else(|| token.strip_prefix("HEADER.D="))
            {
                signing_domain = Some(value.trim_matches('"').trim_end_matches('.'));
            }
        }
        passed && signing_domain.is_some_and(|domain| domain.eq_ignore_ascii_case(sender_domain))
    })
}

fn remove_comments(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let mut depth = 0_u32;
    let mut escaped = false;
    for character in value.chars() {
        if escaped {
            escaped = false;
            if depth == 0 {
                output.push(character);
            }
            continue;
        }
        if character == '\\' {
            escaped = true;
            if depth == 0 {
                output.push(character);
            }
            continue;
        }
        match character {
            '(' => depth = depth.saturating_add(1),
            ')' => depth = depth.saturating_sub(1),
            _ if depth == 0 => output.push(character),
            _ => {}
        }
    }
    output
}

fn lookup_forwarding_user(socket: &Path, address: &str) -> Result<String> {
    if address.contains(['\r', '\n', '\0']) {
        bail!("invalid external sender address");
    }
    let mut stream = UnixStream::connect(socket)
        .with_context(|| format!("connect forwarding map {}", socket.display()))?;
    stream
        .write_all(format!("{address}\n").as_bytes())
        .context("query forwarding map")?;
    let mut response = String::new();
    BufReader::new(stream)
        .take(MAX_LOOKUP_RESPONSE_BYTES + 1)
        .read_line(&mut response)
        .context("read forwarding map response")?;
    if response.len() as u64 > MAX_LOOKUP_RESPONSE_BYTES {
        bail!("forwarding map response is too large");
    }
    let Some(username) = response.trim_end().strip_prefix("OK ") else {
        bail!("external From address is not registered in a local .forward file");
    };
    validate_local_username(username)?;
    Ok(username.to_owned())
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
        MAIL_DOMAIN, REPLY_MARKER, Route, authenticated_username, authentication_result_passes,
        parse_mail_post, parse_recipient, render_message, reply_text, verified_external_sender,
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
    fn external_sender_requires_local_aligned_dkim_result() {
        let message = b"From: Alice <alice@example.com>\r\n\
            Authentication-Results: mail.salyut.one; dkim=pass (good) header.d=example.com header.i=@example.com\r\n\
            Subject: External\r\n\
            Message-ID: <external@example.com>\r\n\
            Content-Type: text/plain\r\n\
            \r\n\
            Verified.\r\n";
        let parsed = mailparse::parse_mail(message).unwrap();
        assert_eq!(
            verified_external_sender(&parsed).unwrap(),
            "alice@example.com"
        );

        let forged = b"From: Alice <alice@example.com>\r\n\
            Authentication-Results: attacker.example; dkim=pass header.d=example.com\r\n\
            Subject: Forged\r\n\
            Message-ID: <forged@example.com>\r\n\
            Content-Type: text/plain\r\n\
            \r\n\
            Forged.\r\n";
        assert!(verified_external_sender(&mailparse::parse_mail(forged).unwrap()).is_err());
    }

    #[test]
    fn dkim_result_must_align_with_the_from_domain() {
        assert!(authentication_result_passes(
            "mail.salyut.one; dkim=pass header.d=example.com header.i=@example.com",
            "example.com"
        ));
        assert!(!authentication_result_passes(
            "mail.salyut.one; dkim=pass header.d=sender.example",
            "example.com"
        ));
        assert!(!authentication_result_passes(
            "mail.salyut.one; dkim=fail (dkim=pass header.d=example.com) header.d=example.com",
            "example.com"
        ));
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
