use std::{net::ToSocketAddrs, path::PathBuf};

use anyhow::{Context, Result};
use clap::Parser;
use salyut_bbs::{
    client::Client,
    protocol::{Board, Post, PostSummary, ProposalState},
};
use tiny_http::{Header, Request as HttpRequest, Response as HttpResponse, Server, StatusCode};

#[derive(Parser)]
#[command(version, about = "Read-only web viewer for Salyut BBS")]
struct Arguments {
    #[arg(long, default_value_os_t = salyut_bbs::paths::read_only_socket())]
    socket: PathBuf,
    #[arg(long, default_value = "127.0.0.1:8080")]
    listen: String,
}

fn main() -> Result<()> {
    let arguments = Arguments::parse();
    arguments
        .listen
        .to_socket_addrs()
        .with_context(|| format!("invalid listen address {}", arguments.listen))?
        .next()
        .context("listen address resolved to no addresses")?;
    let server = Server::http(&arguments.listen)
        .map_err(|error| anyhow::anyhow!("listen on {}: {error}", arguments.listen))?;
    eprintln!("salyut-bbs-web listening on http://{}", arguments.listen);
    let client = Client::new(arguments.socket);
    for request in server.incoming_requests() {
        serve(request, &client);
    }
    Ok(())
}

fn serve(request: HttpRequest, client: &Client) {
    if request.method() != &tiny_http::Method::Get && request.method() != &tiny_http::Method::Head {
        respond(
            request,
            StatusCode(405),
            page("Method not allowed", "<h1>Method not allowed</h1>"),
        );
        return;
    }

    let path = request.url().split('?').next().unwrap_or("/");
    let result = match path {
        "/" => render_index(client),
        "/healthz" => client.identity().map(|_| "ok\n".to_owned()),
        _ if path.starts_with("/posts/") => {
            let Ok(id) = path.trim_start_matches("/posts/").parse::<i64>() else {
                respond(
                    request,
                    StatusCode(404),
                    page("Not found", "<h1>Not found</h1>"),
                );
                return;
            };
            match render_post(client, id) {
                Ok(Some(body)) => Ok(body),
                Ok(None) => {
                    respond(
                        request,
                        StatusCode(404),
                        page("Not found", "<h1>Post not found</h1>"),
                    );
                    return;
                }
                Err(error) => Err(error),
            }
        }
        _ if path.starts_with("/boards/") => {
            let slug = path.trim_start_matches("/boards/");
            match render_board(client, slug) {
                Ok(Some(body)) => Ok(body),
                Ok(None) => {
                    respond(
                        request,
                        StatusCode(404),
                        page("Not found", "<h1>Board not found</h1>"),
                    );
                    return;
                }
                Err(error) => Err(error),
            }
        }
        _ => {
            respond(
                request,
                StatusCode(404),
                page("Not found", "<h1>Not found</h1>"),
            );
            return;
        }
    };

    match result {
        Ok(body) => respond(request, StatusCode(200), body),
        Err(error) => {
            eprintln!("request failed: {error:#}");
            respond(
                request,
                StatusCode(502),
                page(
                    "BBS unavailable",
                    "<h1>BBS unavailable</h1><p>Try again later.</p>",
                ),
            );
        }
    }
}

fn render_index(client: &Client) -> Result<String> {
    let boards = client.boards()?;
    let content = boards.iter().map(board_card).collect::<Vec<_>>().join("");
    Ok(page(
        "Message board",
        &format!(
            "<h1>Bulletin Board System</h1>\
             <p>Browse posts here, or log in over SSH and run <code>bbs</code> \
             to post, reply, and vote.</p><hr>\
             <h2>Boards</h2><ul class=\"boards\">{content}</ul>"
        ),
    ))
}

fn board_card(board: &Board) -> String {
    let restriction = board
        .write_group
        .as_ref()
        .map(|group| {
            format!(
                " <small>(starting threads requires the {} group)</small>",
                escape(group)
            )
        })
        .unwrap_or_default();
    format!(
        "<li><a href=\"/boards/{slug}\">[{name}]</a> — {description}{restriction}</li>",
        slug = escape(&board.slug),
        name = escape(&board.name),
        description = escape(&board.description),
    )
}

fn render_board(client: &Client, slug: &str) -> Result<Option<String>> {
    let boards = client.boards()?;
    let Some(board) = boards.into_iter().find(|board| board.slug == slug) else {
        return Ok(None);
    };
    let posts = client.posts(&board.slug, 200, 0)?;
    let rows = if posts.is_empty() {
        "<p class=\"empty\">No posts yet.</p>".to_owned()
    } else {
        format!(
            "<ol class=\"posts\">{}</ol>",
            posts.iter().map(post_row).collect::<Vec<_>>().join("")
        )
    };
    Ok(Some(page(
        &board.name,
        &format!(
            "<h1>{name}</h1><p>{description}</p><hr>\
             <h2>Posts</h2>{rows}",
            name = escape(&board.name),
            description = escape(&board.description),
        ),
    )))
}

fn post_row(post: &PostSummary) -> String {
    let poll = if post.is_poll { " ◉" } else { "" };
    let proposal = post
        .proposal_state
        .map(|state| format!(" [{}]", state.label()))
        .unwrap_or_default();
    let locked = if post.locked { " [locked]" } else { "" };
    format!(
        "<li><a href=\"/posts/{id}\">{title}{poll}</a>{proposal}{locked} — \
         <span>@{author}, {date}, #{id} · {reply_count} repl{reply_suffix}</span></li>",
        id = post.id,
        title = escape(&post.title),
        poll = poll,
        proposal = proposal,
        locked = locked,
        author = escape(&post.author),
        date = post.updated_at.format("%Y-%m-%d"),
        reply_count = post.reply_count,
        reply_suffix = if post.reply_count == 1 { "y" } else { "ies" },
    )
}

fn render_post(client: &Client, id: i64) -> Result<Option<String>> {
    let Some(post) = client.post(id)? else {
        return Ok(None);
    };
    Ok(Some(page(&post.title, &post_html(&post))))
}

fn post_html(post: &Post) -> String {
    let locked = if post.locked {
        "<p class=\"locked\">[Locked]</p>"
    } else {
        ""
    };
    let proposal = post.proposal.as_ref().map_or_else(String::new, |proposal| {
        let timing = if proposal.state == ProposalState::Voting {
            format!(
                "<p>Voting closes {}.</p>",
                proposal.closes_at.format("%Y-%m-%d %H:%M UTC")
            )
        } else {
            proposal.closed_at.map_or_else(String::new, |closed_at| {
                format!(
                    "<p>Voting closed {}.</p>",
                    closed_at.format("%Y-%m-%d %H:%M UTC")
                )
            })
        };
        let history = proposal
            .events
            .iter()
            .map(|event| {
                let actor = event.actor.as_ref().map_or_else(
                    || "system".to_owned(),
                    |actor| {
                        event.actor_uid.map_or_else(
                            || format!("@{actor}"),
                            |uid| format!("@{actor} (uid {uid})"),
                        )
                    },
                );
                let transition = event.from_state.map_or_else(
                    || event.to_state.label().to_owned(),
                    |from| format!("{} → {}", from.label(), event.to_state.label()),
                );
                let reason = event
                    .reason
                    .as_ref()
                    .map(|reason| format!(" — {}", escape(reason)))
                    .unwrap_or_default();
                format!(
                    "<li>{date} · {transition} · {actor}{reason}</li>",
                    date = event.created_at.format("%Y-%m-%d %H:%M UTC"),
                    transition = escape(&transition),
                    actor = escape(&actor),
                    reason = reason,
                )
            })
            .collect::<Vec<_>>()
            .join("");
        format!(
            "<section class=\"proposal\"><hr><h2>Proposal: {state}</h2>{timing}\
             <h3>History</h3><ol>{history}</ol></section>",
            state = escape(proposal.state.label()),
        )
    });
    let poll = post.poll.as_ref().map_or_else(String::new, |poll| {
        let options = poll
            .options
            .iter()
            .map(|option| {
                let percent = (u64::from(option.votes) * 100)
                    .checked_div(u64::from(poll.total_votes))
                    .unwrap_or(0);
                format!(
                    "<li>{} — {} vote(s), {}%<br>\
                     <div class=\"poll-meter\" role=\"img\" aria-label=\"{} percent\">\
                     <span style=\"width: {}%\"></span></div></li>",
                    escape(&option.label),
                    option.votes,
                    percent,
                    percent,
                    percent,
                )
            })
            .collect::<Vec<_>>()
            .join("");
        let voting = if post
            .proposal
            .as_ref()
            .is_some_and(|proposal| proposal.state == ProposalState::Voting)
        {
            "Log in over SSH and run bbs to vote."
        } else {
            "Voting is closed."
        };
        format!(
            "<section class=\"poll\"><hr><h2>Poll results</h2><ul>{options}</ul>\
             <p>{} total vote(s). {voting}</p></section>",
            poll.total_votes,
        )
    });
    let replies = post
        .replies
        .iter()
        .map(|reply| {
            format!(
                "<article class=\"reply\" id=\"reply-{id}\"><p class=\"byline\">\
                 <a href=\"#reply-{id}\">#{id}</a> · @{author} · {date}</p>\
                 <pre>{body}</pre></article>",
                id = reply.id,
                author = escape(&reply.author),
                date = reply.updated_at.format("%Y-%m-%d %H:%M UTC"),
                body = escape(&reply.body),
            )
        })
        .collect::<Vec<_>>()
        .join("");
    let replies = if replies.is_empty() {
        "<p>No replies yet.</p>".to_owned()
    } else {
        replies
    };
    let reply_status = if post.locked {
        "<p>Replies are closed.</p>"
    } else {
        "<p>Log in over SSH and run <code>bbs</code> to reply.</p>"
    };
    format!(
        "<h1>{title}</h1>{locked}<p class=\"byline\">\
         Posted by @{author} in {board_name} on {date} · #{id}</p>\
         <hr><pre>{body}</pre>{proposal}{poll}<section class=\"replies\"><hr>\
         <h2>Replies</h2>{replies}{reply_status}</section>",
        locked = locked,
        board_name = escape(&post.board.name),
        id = post.id,
        title = escape(&post.title),
        author = escape(&post.author),
        date = post.updated_at.format("%Y-%m-%d %H:%M UTC"),
        body = escape(&post.body),
        proposal = proposal,
        poll = poll,
        replies = replies,
        reply_status = reply_status,
    )
}

fn page(title: &str, content: &str) -> String {
    format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
         <title>{} · salyut.one BBS</title><style>{}</style></head>\
         <body><main><nav class=\"toplinks\"><a href=\"/\">[BBS Home]</a> \
         <a href=\"https://salyut.one\">[salyut.one]</a> \
         <a href=\"https://salyut.one/users\">[User List]</a></nav>{}\
         <hr><footer class=\"footer\">Want to take part? Log in over SSH and run \
         <code>bbs</code> to post, reply, and vote.</footer></main></body></html>",
        escape(title),
        CSS,
        content
    )
}

fn respond(request: HttpRequest, status: StatusCode, body: String) {
    let content_type =
        Header::from_bytes("Content-Type", "text/html; charset=utf-8").expect("valid header");
    let response = HttpResponse::from_string(body)
        .with_status_code(status)
        .with_header(content_type);
    if let Err(error) = request.respond(response) {
        eprintln!("HTTP response error: {error}");
    }
}

fn escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

const CSS: &str = include_str!("../../assets/web.css");

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use salyut_bbs::protocol::{Board, BoardKind, Post, Proposal, ProposalEvent, ProposalState};

    use super::{escape, page, post_html};

    #[test]
    fn html_escapes_untrusted_post_content() {
        assert_eq!(
            escape("<script>alert('x') & \"y\"</script>"),
            "&lt;script&gt;alert(&#39;x&#39;) &amp; &quot;y&quot;&lt;/script&gt;"
        );
    }

    #[test]
    fn post_page_has_no_redundant_board_link() {
        let now = Utc::now();
        let post = Post {
            id: 1,
            board: Board {
                id: 1,
                slug: "general".to_owned(),
                name: "General".to_owned(),
                description: String::new(),
                kind: BoardKind::Discussion,
                write_group: None,
            },
            author: "alice".to_owned(),
            title: "Hello".to_owned(),
            body: "World".to_owned(),
            locked: false,
            replies: Vec::new(),
            poll: None,
            proposal: None,
            created_at: now,
            updated_at: now,
        };
        let html = post_html(&post);
        assert!(!html.contains("href=\"/boards/general\""));
        assert!(html.contains("Posted by @alice in General"));
    }

    #[test]
    fn global_navigation_links_to_user_list() {
        let html = page("Title", "<p>Body</p>");
        assert!(html.contains("href=\"https://salyut.one/users\">[User List]</a>"));
        assert!(html.contains("Want to take part?"));
        assert!(html.contains("<code>bbs</code>"));
        assert!(!html.contains("terminal members"));
        assert!(!html.contains("<code>salyut-bbs</code>"));
        assert!(!html.contains("Service Status"));
    }

    #[test]
    fn proposal_page_renders_state_deadline_and_escaped_history() {
        let now = Utc::now();
        let post = Post {
            id: 3,
            board: Board {
                id: 3,
                slug: "proposals".to_owned(),
                name: "Proposals".to_owned(),
                description: String::new(),
                kind: BoardKind::Polls,
                write_group: None,
            },
            author: "alice".to_owned(),
            title: "Add tea".to_owned(),
            body: "Tea for everyone.".to_owned(),
            locked: false,
            replies: Vec::new(),
            poll: None,
            proposal: Some(Proposal {
                state: ProposalState::Vetoed,
                opens_at: now,
                closes_at: now,
                closed_at: Some(now),
                events: vec![ProposalEvent {
                    id: 1,
                    from_state: Some(ProposalState::Accepted),
                    to_state: ProposalState::Vetoed,
                    actor_uid: Some(0),
                    actor: Some("root".to_owned()),
                    reason: Some("<unsafe>".to_owned()),
                    created_at: now,
                }],
            }),
            created_at: now,
            updated_at: now,
        };

        let html = post_html(&post);
        assert!(html.contains("Proposal: vetoed"));
        assert!(html.contains("@root (uid 0)"));
        assert!(html.contains("&lt;unsafe&gt;"));
        assert!(!html.contains("<unsafe>"));
    }
}
