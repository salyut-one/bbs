use ratatui::{
    layout::{Constraint, Direction, Layout, Margin, Position, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap},
};

use super::{App, ConfirmAction, Editor, EditorField, Mode};

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
        Mode::Edit(editor) => draw_editor(frame, editor, centered(area, 90, 90)),
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
    frame.render_widget(
        Paragraph::new(format!(
            "/{} · {}{}",
            board.slug, board.description, restriction
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
    let help =
        if vote_selected.is_some() {
            "↑/↓: choose · Enter: vote/change vote · Esc: cancel".to_owned()
        } else {
            let mut commands = vec!["Esc/q: close".to_owned()];
            if !post.locked {
                commands.push("a: reply".to_owned());
            }
            if post.proposal.as_ref().is_some_and(|proposal| {
                proposal.state == salyut_bbs::protocol::ProposalState::Voting
            }) {
                commands.push("v: vote".to_owned());
            }
            if post.author == app.handle && post.proposal.is_none() {
                commands.push("e: edit post".to_owned());
            }
            if post.author == app.handle
                && post.proposal.as_ref().is_some_and(|proposal| {
                    proposal.state == salyut_bbs::protocol::ProposalState::Voting
                })
            {
                commands.push("w: withdraw".to_owned());
            }
            if !post.replies.is_empty() {
                commands.push("↑/↓: select reply".to_owned());
                if post
                    .replies
                    .get(app.reply_selected)
                    .is_some_and(|reply| reply.author == app.handle)
                {
                    commands.push("u: update reply".to_owned());
                    commands.push("d: delete reply".to_owned());
                }
            }
            if app.groups.iter().any(|group| group == "wheel") {
                commands.push(if post.locked {
                    "l: unlock".to_owned()
                } else {
                    "l: lock".to_owned()
                });
                if post.proposal.as_ref().is_some_and(|proposal| {
                    proposal.state == salyut_bbs::protocol::ProposalState::Accepted
                }) {
                    commands.push("x: veto".to_owned());
                    commands.push("i: implemented".to_owned());
                }
            }
            commands.join(" · ")
        };
    lines.push(Line::styled(help, Style::default().fg(Color::Gray)));
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .wrap(Wrap { trim: false })
            .block(Block::default().title(" post ").borders(Borders::ALL)),
        area,
    );
}

fn draw_editor(frame: &mut ratatui::Frame<'_>, editor: &Editor, area: Rect) {
    frame.render_widget(Clear, area);
    if editor.target.is_reply() || editor.target.is_note() {
        let sections = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(5), Constraint::Length(3)])
            .split(area);
        let title = match editor.target {
            super::EditorTarget::VetoProposal(_) => " veto reason ",
            super::EditorTarget::ImplementProposal(_) => " implementation note ",
            _ => " reply ",
        };
        draw_editor_field(
            frame,
            &editor.body,
            editor.body_cursor,
            title,
            true,
            sections[0],
        );
        frame.render_widget(
            Paragraph::new(Text::from(vec![
                Line::raw(format!("/{}", editor.board_slug)),
                Line::styled(
                    "arrows/Home/End: move · Ctrl-S: save · Esc: cancel",
                    Style::default().fg(Color::Gray),
                ),
            ])),
            sections[1],
        );
        return;
    }

    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(5),
            Constraint::Length(3),
        ])
        .split(area);
    draw_editor_field(
        frame,
        &editor.title,
        editor.title_cursor,
        " title ",
        editor.field == EditorField::Title,
        sections[0],
    );
    draw_editor_field(
        frame,
        &editor.body,
        editor.body_cursor,
        " body ",
        editor.field == EditorField::Body,
        sections[1],
    );
    frame.render_widget(
        Paragraph::new(Text::from(vec![
            Line::raw(format!("/{} · Tab: switch field", editor.board_slug)),
            Line::styled(
                "arrows/Home/End: move · Ctrl-S: save · Esc: cancel",
                Style::default().fg(Color::Gray),
            ),
        ])),
        sections[2],
    );
}

fn draw_editor_field(
    frame: &mut ratatui::Frame<'_>,
    value: &str,
    cursor: usize,
    title: &'static str,
    selected: bool,
    area: Rect,
) {
    let inner = area.inner(Margin {
        horizontal: 1,
        vertical: 1,
    });
    let (scroll, cursor_position) = editor_view(value, cursor, inner);
    frame.render_widget(
        Paragraph::new(value)
            .scroll(scroll)
            .block(editor_block(title, selected)),
        area,
    );
    if selected && inner.width > 0 && inner.height > 0 {
        frame.set_cursor_position(cursor_position);
    }
}

fn editor_view(value: &str, cursor: usize, area: Rect) -> ((u16, u16), Position) {
    let cursor = cursor.min(value.len());
    let line_start = value[..cursor].rfind('\n').map_or(0, |index| index + 1);
    let line = value[..cursor]
        .bytes()
        .filter(|byte| *byte == b'\n')
        .count();
    let column = Line::raw(&value[line_start..cursor]).width();
    let vertical_scroll = line.saturating_sub(usize::from(area.height.saturating_sub(1)));
    let horizontal_scroll = column.saturating_sub(usize::from(area.width.saturating_sub(1)));
    let x = area
        .x
        .saturating_add((column - horizontal_scroll).min(usize::from(u16::MAX)) as u16);
    let y = area
        .y
        .saturating_add((line - vertical_scroll).min(usize::from(u16::MAX)) as u16);
    (
        (
            vertical_scroll.min(usize::from(u16::MAX)) as u16,
            horizontal_scroll.min(usize::from(u16::MAX)) as u16,
        ),
        Position::new(x, y),
    )
}

fn editor_block(title: &'static str, selected: bool) -> Block<'static> {
    Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(if selected {
            Style::default().fg(Color::Gray)
        } else {
            Style::default()
        })
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn editor_cursor_scrolls_into_the_visible_area() {
        let value = "first\nsecond\nthird";
        let cursor = "first\nsecond\nthi".len();
        let area = Rect::new(10, 5, 3, 2);

        let (scroll, position) = editor_view(value, cursor, area);

        assert_eq!(scroll, (1, 1));
        assert_eq!(position, Position::new(12, 6));
    }
}
