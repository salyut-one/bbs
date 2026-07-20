use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap},
};

use super::{App, ConfirmAction, Mode};

pub(super) fn draw(frame: &mut ratatui::Frame<'_>, app: &App) {
    let area = frame.area();
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Min(5),
            Constraint::Length(4),
        ])
        .split(area);

    draw_header(frame, app, rows[0]);
    draw_board_description(frame, app, rows[1]);
    draw_post_list(frame, app, rows[2]);
    draw_help(frame, app, rows[3]);

    match &app.mode {
        Mode::View => draw_post(frame, app, centered(area, 88, 84), None),
        Mode::Vote(selected) => draw_post(frame, app, centered(area, 88, 84), Some(*selected)),
        Mode::Confirm(action, yes) => {
            draw_confirmation(frame, app, *action, *yes, centered(area, 54, 24))
        }
        Mode::Browse => {}
    }
}

fn draw_header(frame: &mut ratatui::Frame<'_>, app: &App, area: Rect) {
    let mut line = vec![
        Span::styled(
            " salyut.one bbs ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Gray)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(format!("  @{}  ", app.handle)),
    ];
    for (index, board) in app.boards.iter().enumerate() {
        let style = if index == app.board_selected {
            Style::default()
                .fg(Color::Black)
                .bg(Color::Gray)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Gray)
        };
        line.push(Span::styled(format!(" {} ", board.name), style));
        line.push(Span::raw(" "));
    }
    frame.render_widget(
        Paragraph::new(Line::from(line)).block(Block::default().borders(Borders::ALL)),
        area,
    );
}

fn draw_board_description(frame: &mut ratatui::Frame<'_>, app: &App, area: Rect) {
    let board = app.current_board();
    let restriction = board
        .write_group
        .as_ref()
        .map(|group| format!(" · new threads: {group}"))
        .unwrap_or_default();
    let mail = if !app.mail_eligible {
        " · mail: ineligible"
    } else if app.mail_subscriptions[app.board_selected] {
        " · mail: subscribed"
    } else {
        " · mail: unsubscribed"
    };
    frame.render_widget(
        Paragraph::new(format!(
            "/{} · {}{}{}",
            board.slug, board.description, restriction, mail
        ))
        .block(Block::default().borders(Borders::ALL)),
        area,
    );
}

fn draw_post_list(frame: &mut ratatui::Frame<'_>, app: &App, area: Rect) {
    let items = app.posts.iter().map(post_item).collect::<Vec<_>>();
    let mut state =
        ListState::default().with_selected((!app.posts.is_empty()).then_some(app.selected));
    frame.render_stateful_widget(
        List::new(items)
            .block(
                Block::default()
                    .title(format!(" {} ", app.current_board().name))
                    .borders(Borders::ALL),
            )
            .highlight_style(
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Gray)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("» "),
        area,
        &mut state,
    );
}

fn post_item(post: &salyut_bbs::protocol::PostSummary) -> ListItem<'_> {
    let proposal = post
        .proposal_state
        .map(|state| format!(" [{}]", state.label()))
        .unwrap_or_default();
    ListItem::new(Line::from(vec![
        Span::styled(
            format!("{:>4} ", post.id),
            Style::default().fg(Color::DarkGray),
        ),
        Span::styled(
            format!(
                "{}{}{}{}",
                post.title,
                if post.is_poll { " ◉" } else { "" },
                proposal,
                if post.locked { " [locked]" } else { "" }
            ),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::raw(format!(
            "  @{}  {}  {} repl{}",
            post.author,
            post.updated_at.format("%Y-%m-%d"),
            post.reply_count,
            if post.reply_count == 1 { "y" } else { "ies" },
        )),
    ]))
}

fn draw_help(frame: &mut ratatui::Frame<'_>, app: &App, area: Rect) {
    frame.render_widget(
        Paragraph::new(Text::from(vec![
            Line::from(vec![
                key("←/→"),
                Span::raw(" boards  "),
                key("↑/↓"),
                Span::raw(" select  "),
                key("enter"),
                Span::raw(" open  "),
                key("n"),
                Span::raw(" new  "),
                key("e"),
                Span::raw(" edit  "),
                key("d"),
                Span::raw(" delete  "),
                key("r"),
                Span::raw(" refresh  "),
                key("m"),
                Span::raw(" toggle mail  "),
                key("q"),
                Span::raw(" quit"),
            ]),
            Line::styled(
                format!("status: {}", app.message),
                Style::default().fg(Color::Gray),
            ),
        ]))
        .block(Block::default().borders(Borders::ALL)),
        area,
    );
}

fn key(label: &'static str) -> Span<'static> {
    Span::styled(
        format!(" {label} "),
        Style::default()
            .fg(Color::Black)
            .bg(Color::Gray)
            .add_modifier(Modifier::BOLD),
    )
}

fn draw_post(frame: &mut ratatui::Frame<'_>, app: &App, area: Rect, vote_selected: Option<usize>) {
    let Some(post) = &app.viewed else { return };
    frame.render_widget(Clear, area);
    let mut lines = vec![
        Line::styled(
            post.title.clone(),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Line::raw(format!(
            "/{} · by @{} · {} · post #{}",
            post.board.slug,
            post.author,
            post.updated_at.format("%Y-%m-%d %H:%M UTC"),
            post.id
        )),
        Line::raw(""),
        Line::raw(post.body.clone()),
    ];
    if post.locked {
        lines.insert(
            2,
            Line::styled("[locked]", Style::default().fg(Color::Gray)),
        );
    }
    if let Some(proposal) = &post.proposal {
        lines.push(Line::raw(""));
        lines.push(Line::styled(
            format!("proposal · {}", proposal.state.label()),
            Style::default()
                .fg(Color::Gray)
                .add_modifier(Modifier::BOLD),
        ));
        if proposal.state == salyut_bbs::protocol::ProposalState::Voting {
            lines.push(Line::raw(format!(
                "Voting closes {}",
                proposal.closes_at.format("%Y-%m-%d %H:%M UTC")
            )));
        } else if let Some(closed_at) = proposal.closed_at {
            lines.push(Line::raw(format!(
                "Voting closed {}",
                closed_at.format("%Y-%m-%d %H:%M UTC")
            )));
        }
        for event in &proposal.events {
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
                .map(|reason| format!(" — {reason}"))
                .unwrap_or_default();
            lines.push(Line::styled(
                format!(
                    "{} · {} · {}{}",
                    event.created_at.format("%Y-%m-%d %H:%M UTC"),
                    transition,
                    actor,
                    reason
                ),
                Style::default().fg(Color::DarkGray),
            ));
        }
    }
    if let Some(poll) = &post.poll {
        lines.push(Line::raw(""));
        lines.push(Line::styled(
            format!("poll · {} total vote(s)", poll.total_votes),
            Style::default().fg(Color::Gray),
        ));
        for (index, option) in poll.options.iter().enumerate() {
            let marker = if vote_selected == Some(index) {
                "»"
            } else if poll.my_vote == Some(option.id) {
                "●"
            } else {
                " "
            };
            let percent = (u64::from(option.votes) * 100)
                .checked_div(u64::from(poll.total_votes))
                .unwrap_or(0);
            lines.push(Line::raw(format!(
                "{marker} {} — {} vote(s), {percent}%",
                option.label, option.votes
            )));
        }
    }
    lines.push(Line::raw(""));
    lines.push(Line::styled(
        format!("replies · {}", post.replies.len()),
        Style::default().fg(Color::Gray),
    ));
    for (index, reply) in post.replies.iter().enumerate() {
        let marker = if index == app.reply_selected {
            "»"
        } else {
            " "
        };
        lines.push(Line::styled(
            format!(
                "{marker} #{} · @{} · {}",
                reply.id,
                reply.author,
                reply.updated_at.format("%Y-%m-%d %H:%M UTC")
            ),
            Style::default().fg(Color::DarkGray),
        ));
        for body_line in reply.body.lines() {
            lines.push(Line::raw(format!("    {body_line}")));
        }
    }
    lines.push(Line::raw(""));
    let help = if vote_selected.is_some() {
        "↑/↓: choose · Enter: vote/change vote · Esc: cancel"
    } else {
        "Esc/q: close · a: reply · v: vote · e: edit · ↑/↓: select reply · u: edit reply · d: delete reply · l: lock · w: withdraw · x: veto · i: implemented"
    };
    lines.push(Line::styled(help, Style::default().fg(Color::Gray)));
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .wrap(Wrap { trim: false })
            .block(Block::default().title(" post ").borders(Borders::ALL)),
        area,
    );
}

fn draw_confirmation(
    frame: &mut ratatui::Frame<'_>,
    app: &App,
    action: ConfirmAction,
    yes: bool,
    area: Rect,
) {
    frame.render_widget(Clear, area);
    let question = match action {
        ConfirmAction::DeletePost => {
            let title = app
                .selected_post()
                .map(|post| post.title.as_str())
                .unwrap_or("this post");
            format!("Delete “{title}”?")
        }
        ConfirmAction::DeleteReply(id) => format!("Delete reply #{id}?"),
        ConfirmAction::SetLocked(locked) => {
            let title = app
                .viewed
                .as_ref()
                .map(|post| post.title.as_str())
                .unwrap_or("this post");
            if locked {
                format!("Lock “{title}”?")
            } else {
                format!("Unlock “{title}”?")
            }
        }
        ConfirmAction::WithdrawProposal => {
            let title = app
                .viewed
                .as_ref()
                .map(|post| post.title.as_str())
                .unwrap_or("this proposal");
            format!("Withdraw “{title}”?")
        }
    };
    let selected = Style::default()
        .fg(Color::Black)
        .bg(Color::Gray)
        .add_modifier(Modifier::BOLD);
    let normal = Style::default().fg(Color::Gray);
    frame.render_widget(
        Paragraph::new(Text::from(vec![
            Line::raw(question),
            Line::raw(""),
            Line::from(vec![
                Span::styled(" Yes ", if yes { selected } else { normal }),
                Span::raw("    "),
                Span::styled(" No ", if yes { normal } else { selected }),
            ]),
            Line::raw(""),
            Line::styled(
                "←/→ or Tab: choose · Enter: confirm · Esc: cancel",
                Style::default().fg(Color::Gray),
            ),
        ]))
        .wrap(Wrap { trim: true })
        .block(Block::default().title(" confirm ").borders(Borders::ALL)),
        area,
    );
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let rows = Layout::vertical([
        Constraint::Percentage((100 - height) / 2),
        Constraint::Percentage(height),
        Constraint::Percentage((100 - height) / 2),
    ])
    .split(area);
    Layout::horizontal([
        Constraint::Percentage((100 - width) / 2),
        Constraint::Percentage(width),
        Constraint::Percentage((100 - width) / 2),
    ])
    .split(rows[1])[1]
}
