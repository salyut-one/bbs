use std::{io, path::PathBuf, time::Duration};

use anyhow::{Result, bail};
use clap::Parser;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use ratatui::{Terminal, backend::CrosstermBackend};
use salyut_bbs::{
    client::Client,
    protocol::{Board, BoardKind, Post, PostSummary, ProposalState},
};

mod ui;

#[derive(Parser)]
#[command(version, about = "salyut.one BBS client")]
struct Arguments {
    #[arg(long, default_value_os_t = salyut_bbs::paths::read_write_socket())]
    socket: PathBuf,
}

enum Mode {
    Browse,
    View,
    Edit(Editor),
    Vote(usize),
    Confirm(ConfirmAction, bool),
}

#[derive(Clone, Copy)]
enum ConfirmAction {
    DeletePost,
    DeleteReply(i64),
    SetLocked(bool),
    WithdrawProposal,
}

struct Editor {
    target: EditorTarget,
    board_slug: String,
    title: String,
    body: String,
    creates_proposal: bool,
    field: EditorField,
    title_cursor: usize,
    body_cursor: usize,
}

#[derive(Clone, Copy)]
enum EditorTarget {
    NewPost,
    EditPost(i64),
    NewReply(i64),
    EditReply { id: i64 },
    VetoProposal(i64),
    ImplementProposal(i64),
}

impl EditorTarget {
    fn is_reply(&self) -> bool {
        matches!(self, Self::NewReply(_) | Self::EditReply { .. })
    }

    fn returns_to_view(&self) -> bool {
        matches!(
            self,
            Self::NewReply(_)
                | Self::EditReply { .. }
                | Self::VetoProposal(_)
                | Self::ImplementProposal(_)
        )
    }

    fn is_note(&self) -> bool {
        matches!(self, Self::VetoProposal(_) | Self::ImplementProposal(_))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EditorField {
    Title,
    Body,
}

impl Editor {
    fn new(
        target: EditorTarget,
        board_slug: String,
        title: String,
        body: String,
        creates_proposal: bool,
    ) -> Self {
        let field = if target.is_reply() || target.is_note() {
            EditorField::Body
        } else {
            EditorField::Title
        };
        let title_cursor = title.len();
        let body_cursor = body.len();
        Self {
            target,
            board_slug,
            title,
            body,
            creates_proposal,
            field,
            title_cursor,
            body_cursor,
        }
    }
}

struct App {
    client: Client,
    handle: String,
    groups: Vec<String>,
    boards: Vec<Board>,
    board_selected: usize,
    posts: Vec<PostSummary>,
    selected: usize,
    reply_selected: usize,
    viewed: Option<Post>,
    mode: Mode,
    message: String,
    quit: bool,
}

fn main() -> Result<()> {
    let arguments = Arguments::parse();
    let client = Client::new(arguments.socket);
    let identity = client.identity()?;
    let boards = client.boards()?;
    if boards.is_empty() {
        bail!("daemon returned no boards");
    }
    let mut app = App {
        client,
        handle: identity.handle,
        groups: identity.groups,
        boards,
        board_selected: 0,
        posts: Vec::new(),
        selected: 0,
        reply_selected: 0,
        viewed: None,
        mode: Mode::Browse,
        message: String::new(),
        quit: false,
    };
    app.refresh();

    crossterm::terminal::enable_raw_mode()?;
    let mut stdout = io::stdout();
    crossterm::execute!(
        stdout,
        crossterm::terminal::EnterAlternateScreen,
        crossterm::cursor::Hide
    )?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    let result = run(&mut terminal, &mut app);
    crossterm::terminal::disable_raw_mode()?;
    crossterm::execute!(
        terminal.backend_mut(),
        crossterm::terminal::LeaveAlternateScreen,
        crossterm::cursor::Show
    )?;
    result
}

fn run(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>, app: &mut App) -> Result<()> {
    while !app.quit {
        terminal.draw(|frame| ui::draw(frame, app))?;
        if event::poll(Duration::from_millis(250))?
            && let Event::Key(key) = event::read()?
            && key.kind == crossterm::event::KeyEventKind::Press
        {
            handle_key(app, key);
        }
    }
    Ok(())
}

fn handle_key(app: &mut App, key: KeyEvent) {
    let mode = std::mem::replace(&mut app.mode, Mode::Browse);
    app.mode = match mode {
        Mode::Browse => handle_browse_key(app, key),
        Mode::View => handle_view_key(app, key),
        Mode::Vote(mut selected) => match key.code {
            KeyCode::Esc | KeyCode::Char('q') => Mode::View,
            KeyCode::Down | KeyCode::Char('j') => {
                if let Some(poll) = app.viewed.as_ref().and_then(|post| post.poll.as_ref())
                    && selected + 1 < poll.options.len()
                {
                    selected += 1;
                }
                Mode::Vote(selected)
            }
            KeyCode::Up | KeyCode::Char('k') => Mode::Vote(selected.saturating_sub(1)),
            KeyCode::Enter => {
                app.cast_vote(selected);
                Mode::View
            }
            _ => Mode::Vote(selected),
        },
        Mode::Confirm(action, mut yes) => match key.code {
            KeyCode::Left
            | KeyCode::Right
            | KeyCode::Up
            | KeyCode::Down
            | KeyCode::Tab
            | KeyCode::BackTab => {
                yes = !yes;
                Mode::Confirm(action, yes)
            }
            KeyCode::Enter => {
                if yes {
                    match action {
                        ConfirmAction::DeletePost => app.delete_selected(),
                        ConfirmAction::DeleteReply(id) => app.delete_reply(id),
                        ConfirmAction::SetLocked(locked) => app.set_locked(locked),
                        ConfirmAction::WithdrawProposal => app.withdraw_proposal(),
                    }
                }
                confirmation_destination(action)
            }
            KeyCode::Esc | KeyCode::Char('q') => confirmation_destination(action),
            _ => Mode::Confirm(action, yes),
        },
        Mode::Edit(mut editor) => {
            if key.code == KeyCode::Esc {
                if editor.target.returns_to_view() {
                    Mode::View
                } else {
                    Mode::Browse
                }
            } else if key.code == KeyCode::Char('s')
                && key.modifiers.contains(KeyModifiers::CONTROL)
            {
                let returns_to_view = editor.target.returns_to_view();
                if app.save(&editor) {
                    if returns_to_view {
                        Mode::View
                    } else {
                        Mode::Browse
                    }
                } else {
                    Mode::Edit(editor)
                }
            } else {
                edit_key(&mut editor, key);
                Mode::Edit(editor)
            }
        }
    };
}

fn handle_browse_key(app: &mut App, key: KeyEvent) -> Mode {
    match key.code {
        KeyCode::Char('q') => app.quit = true,
        KeyCode::Char('r') => app.refresh(),
        KeyCode::Right | KeyCode::Char(']') | KeyCode::Tab => app.next_board(),
        KeyCode::Left | KeyCode::Char('[') | KeyCode::BackTab => app.previous_board(),
        KeyCode::Char('n') => {
            if !app.can_write_current_board() {
                app.message = app.write_denied_message();
            } else {
                let board = app.current_board();
                return Mode::Edit(Editor::new(
                    EditorTarget::NewPost,
                    board.slug.clone(),
                    String::new(),
                    String::new(),
                    board.kind == BoardKind::Polls,
                ));
            }
        }
        KeyCode::Char('e') => {
            if let Some(post) = app.load_selected() {
                if post.proposal.is_some() {
                    app.message = "Proposals cannot be edited after voting begins".to_owned();
                    return Mode::Browse;
                }
                if post.author == app.handle {
                    return Mode::Edit(Editor::new(
                        EditorTarget::EditPost(post.id),
                        post.board.slug,
                        post.title,
                        post.body,
                        false,
                    ));
                }
                app.message = "You can only edit your own posts".to_owned();
            }
        }
        KeyCode::Char('d') => {
            if app
                .selected_post()
                .is_some_and(|post| post.proposal_state.is_some())
            {
                app.message = "Open a proposal and press w to withdraw it".to_owned();
            } else if app.selected_post().is_some() {
                return Mode::Confirm(ConfirmAction::DeletePost, false);
            }
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if app.selected + 1 < app.posts.len() {
                app.selected += 1;
            }
        }
        KeyCode::Up | KeyCode::Char('k') => {
            app.selected = app.selected.saturating_sub(1);
        }
        KeyCode::Enter => {
            if let Some(post) = app.load_selected() {
                app.viewed = Some(post);
                app.reply_selected = 0;
                return Mode::View;
            }
        }
        _ => {}
    }
    Mode::Browse
}

fn handle_view_key(app: &mut App, key: KeyEvent) -> Mode {
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') | KeyCode::Backspace => Mode::Browse,
        KeyCode::Char('v') => {
            if let Some(proposal) = app.viewed.as_ref().and_then(|post| post.proposal.as_ref())
                && proposal.state != ProposalState::Voting
            {
                app.message = format!(
                    "Voting is closed; this proposal is {}",
                    proposal.state.label()
                );
                return Mode::View;
            }
            if app.viewed.as_ref().is_some_and(|post| {
                post.proposal
                    .as_ref()
                    .is_some_and(|proposal| proposal.state == ProposalState::Voting)
                    && post
                        .poll
                        .as_ref()
                        .is_some_and(|poll| !poll.options.is_empty())
            }) {
                Mode::Vote(0)
            } else {
                app.message = "This post has no poll".to_owned();
                Mode::View
            }
        }
        KeyCode::Char('e') => {
            if let Some(post) = &app.viewed
                && post.author == app.handle
            {
                if post.proposal.is_some() {
                    app.message = "Proposals cannot be edited after voting begins".to_owned();
                    return Mode::View;
                }
                Mode::Edit(Editor::new(
                    EditorTarget::EditPost(post.id),
                    post.board.slug.clone(),
                    post.title.clone(),
                    post.body.clone(),
                    false,
                ))
            } else {
                app.message = "You can only edit your own posts".to_owned();
                Mode::View
            }
        }
        KeyCode::Char('a') => {
            if let Some(post) = &app.viewed {
                if post.locked {
                    app.message = "This post is locked".to_owned();
                    return Mode::View;
                }
                return Mode::Edit(Editor::new(
                    EditorTarget::NewReply(post.id),
                    post.board.slug.clone(),
                    String::new(),
                    String::new(),
                    false,
                ));
            }
            Mode::View
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if let Some(post) = &app.viewed
                && app.reply_selected + 1 < post.replies.len()
            {
                app.reply_selected += 1;
            }
            Mode::View
        }
        KeyCode::Up | KeyCode::Char('k') => {
            app.reply_selected = app.reply_selected.saturating_sub(1);
            Mode::View
        }
        KeyCode::Char('u') => {
            let Some(post) = &app.viewed else {
                return Mode::View;
            };
            let Some(reply) = post.replies.get(app.reply_selected) else {
                app.message = "There is no reply selected".to_owned();
                return Mode::View;
            };
            if reply.author != app.handle {
                app.message = "You can only edit your own replies".to_owned();
                return Mode::View;
            }
            Mode::Edit(Editor::new(
                EditorTarget::EditReply { id: reply.id },
                post.board.slug.clone(),
                String::new(),
                reply.body.clone(),
                false,
            ))
        }
        KeyCode::Char('d') => {
            let Some(reply) = app
                .viewed
                .as_ref()
                .and_then(|post| post.replies.get(app.reply_selected))
            else {
                app.message = "There is no reply selected".to_owned();
                return Mode::View;
            };
            if reply.author != app.handle {
                app.message = "You can only delete your own replies".to_owned();
                return Mode::View;
            }
            Mode::Confirm(ConfirmAction::DeleteReply(reply.id), false)
        }
        KeyCode::Char('l') => {
            if !app.groups.iter().any(|group| group == "wheel") {
                app.message = "Locking posts requires Unix group wheel".to_owned();
                return Mode::View;
            }
            let Some(post) = &app.viewed else {
                return Mode::View;
            };
            Mode::Confirm(ConfirmAction::SetLocked(!post.locked), false)
        }
        KeyCode::Char('w') => {
            let Some(post) = &app.viewed else {
                return Mode::View;
            };
            if post.author != app.handle
                || post.proposal.as_ref().map(|proposal| proposal.state)
                    != Some(ProposalState::Voting)
            {
                app.message =
                    "Only the author may withdraw a proposal while voting is open".to_owned();
                return Mode::View;
            }
            Mode::Confirm(ConfirmAction::WithdrawProposal, false)
        }
        KeyCode::Char('x') | KeyCode::Char('i') => {
            if !app.groups.iter().any(|group| group == "wheel") {
                app.message = "This proposal action requires Unix group wheel".to_owned();
                return Mode::View;
            }
            let Some(post) = &app.viewed else {
                return Mode::View;
            };
            if post.proposal.as_ref().map(|proposal| proposal.state)
                != Some(ProposalState::Accepted)
            {
                app.message = "Only accepted proposals can be vetoed or implemented".to_owned();
                return Mode::View;
            }
            let target = if key.code == KeyCode::Char('x') {
                EditorTarget::VetoProposal(post.id)
            } else {
                EditorTarget::ImplementProposal(post.id)
            };
            Mode::Edit(Editor::new(
                target,
                post.board.slug.clone(),
                String::new(),
                String::new(),
                false,
            ))
        }
        _ => Mode::View,
    }
}

fn confirmation_destination(action: ConfirmAction) -> Mode {
    match action {
        ConfirmAction::DeletePost => Mode::Browse,
        ConfirmAction::DeleteReply(_)
        | ConfirmAction::SetLocked(_)
        | ConfirmAction::WithdrawProposal => Mode::View,
    }
}

fn edit_key(editor: &mut Editor, key: KeyEvent) {
    if (editor.target.is_reply() || editor.target.is_note())
        && matches!(key.code, KeyCode::Tab | KeyCode::BackTab)
    {
        return;
    }
    if matches!(key.code, KeyCode::Tab | KeyCode::BackTab) {
        editor.field = match editor.field {
            EditorField::Title => EditorField::Body,
            EditorField::Body => EditorField::Title,
        };
        return;
    }
    if key.modifiers.contains(KeyModifiers::CONTROL) || key.modifiers.contains(KeyModifiers::SUPER)
    {
        return;
    }

    let field = editor.field;
    let (value, cursor) = active_value_and_cursor(editor);
    *cursor = (*cursor).min(value.len());
    match key.code {
        KeyCode::Left => *cursor = previous_char_boundary(value, *cursor),
        KeyCode::Right => *cursor = next_char_boundary(value, *cursor),
        KeyCode::Home => *cursor = line_start(value, *cursor),
        KeyCode::End => *cursor = line_end(value, *cursor),
        KeyCode::Up => *cursor = vertical_cursor(value, *cursor, -1),
        KeyCode::Down => *cursor = vertical_cursor(value, *cursor, 1),
        KeyCode::Backspace if *cursor > 0 => {
            let previous = previous_char_boundary(value, *cursor);
            value.replace_range(previous..*cursor, "");
            *cursor = previous;
        }
        KeyCode::Delete if *cursor < value.len() => {
            let next = next_char_boundary(value, *cursor);
            value.replace_range(*cursor..next, "");
        }
        KeyCode::Enter if field == EditorField::Body => {
            value.insert(*cursor, '\n');
            *cursor += 1;
        }
        KeyCode::Char(character) => {
            value.insert(*cursor, character);
            *cursor += character.len_utf8();
        }
        _ => {}
    }
}

fn active_value_and_cursor(editor: &mut Editor) -> (&mut String, &mut usize) {
    match editor.field {
        EditorField::Title => (&mut editor.title, &mut editor.title_cursor),
        EditorField::Body => (&mut editor.body, &mut editor.body_cursor),
    }
}

fn previous_char_boundary(value: &str, cursor: usize) -> usize {
    value[..cursor]
        .char_indices()
        .next_back()
        .map_or(0, |(index, _)| index)
}

fn next_char_boundary(value: &str, cursor: usize) -> usize {
    value[cursor..]
        .chars()
        .next()
        .map_or(value.len(), |character| cursor + character.len_utf8())
}

fn line_start(value: &str, cursor: usize) -> usize {
    value[..cursor].rfind('\n').map_or(0, |index| index + 1)
}

fn line_end(value: &str, cursor: usize) -> usize {
    value[cursor..]
        .find('\n')
        .map_or(value.len(), |index| cursor + index)
}

fn vertical_cursor(value: &str, cursor: usize, direction: i8) -> usize {
    let current_start = line_start(value, cursor);
    let column = value[current_start..cursor].chars().count();
    let (target_start, target_end) = if direction < 0 {
        if current_start == 0 {
            return cursor;
        }
        let target_end = current_start - 1;
        let target_start = line_start(value, target_end);
        (target_start, target_end)
    } else {
        let current_end = line_end(value, cursor);
        if current_end == value.len() {
            return cursor;
        }
        let target_start = current_end + 1;
        let target_end = line_end(value, target_start);
        (target_start, target_end)
    };
    byte_at_column(&value[target_start..target_end], column) + target_start
}

fn byte_at_column(line: &str, column: usize) -> usize {
    line.char_indices()
        .nth(column)
        .map_or(line.len(), |(index, _)| index)
}

impl App {
    fn current_board(&self) -> &Board {
        &self.boards[self.board_selected]
    }

    fn can_write_current_board(&self) -> bool {
        self.current_board()
            .write_group
            .as_ref()
            .is_none_or(|group| self.groups.iter().any(|candidate| candidate == group))
    }

    fn write_denied_message(&self) -> String {
        self.current_board().write_group.as_ref().map_or_else(
            || format!("You cannot write to {}", self.current_board().name),
            |group| {
                format!(
                    "Writing to {} requires Unix group {group}",
                    self.current_board().name
                )
            },
        )
    }

    fn next_board(&mut self) {
        self.board_selected = (self.board_selected + 1) % self.boards.len();
        self.selected = 0;
        self.refresh();
    }

    fn previous_board(&mut self) {
        self.board_selected = self
            .board_selected
            .checked_sub(1)
            .unwrap_or(self.boards.len() - 1);
        self.selected = 0;
        self.refresh();
    }

    fn refresh(&mut self) {
        let board = self.current_board().slug.clone();
        match self.client.posts(&board, 200, 0) {
            Ok(posts) => {
                self.posts = posts;
                self.selected = if self.posts.is_empty() {
                    0
                } else {
                    self.selected.min(self.posts.len() - 1)
                };
                self.message = format!("{} post(s)", self.posts.len());
            }
            Err(error) => self.message = error.to_string(),
        }
    }

    fn selected_post(&self) -> Option<&PostSummary> {
        self.posts.get(self.selected)
    }

    fn load_selected(&mut self) -> Option<Post> {
        let id = self.selected_post()?.id;
        match self.client.post(id) {
            Ok(post) => post,
            Err(error) => {
                self.message = error.to_string();
                None
            }
        }
    }

    fn save(&mut self, editor: &Editor) -> bool {
        let result = match editor.target {
            EditorTarget::EditPost(id) => self.client.update_post(id, &editor.title, &editor.body),
            EditorTarget::NewReply(post_id) => self.client.create_reply(post_id, &editor.body),
            EditorTarget::EditReply { id } => self.client.update_reply(id, &editor.body),
            EditorTarget::NewPost if editor.creates_proposal => {
                self.client
                    .create_proposal(&editor.board_slug, &editor.title, &editor.body)
            }
            EditorTarget::VetoProposal(id) => self.client.veto_proposal(id, &editor.body),
            EditorTarget::ImplementProposal(id) => {
                self.client.mark_proposal_implemented(id, &editor.body)
            }
            EditorTarget::NewPost => {
                self.client
                    .create_post(&editor.board_slug, &editor.title, &editor.body)
            }
        };
        match result {
            Ok(post) => {
                self.message = match editor.target {
                    EditorTarget::EditPost(_) => "Post updated",
                    EditorTarget::NewPost if editor.creates_proposal => "Proposal created",
                    EditorTarget::NewPost => "Post created",
                    EditorTarget::NewReply(_) => "Reply posted",
                    EditorTarget::EditReply { .. } => "Reply updated",
                    EditorTarget::VetoProposal(_) => "Proposal vetoed",
                    EditorTarget::ImplementProposal(_) => "Proposal marked implemented",
                }
                .to_owned();
                if editor.target.returns_to_view() {
                    self.viewed = Some(post);
                } else {
                    self.refresh();
                }
                true
            }
            Err(error) => {
                self.message = error.to_string();
                false
            }
        }
    }

    fn delete_reply(&mut self, id: i64) {
        match self.client.delete_reply(id) {
            Ok(post_id) => match self.client.post(post_id) {
                Ok(Some(post)) => {
                    self.reply_selected = self
                        .reply_selected
                        .min(post.replies.len().saturating_sub(1));
                    self.viewed = Some(post);
                    self.message = "Reply deleted".to_owned();
                }
                Ok(None) => self.message = "Post not found".to_owned(),
                Err(error) => self.message = error.to_string(),
            },
            Err(error) => self.message = error.to_string(),
        }
    }

    fn set_locked(&mut self, locked: bool) {
        let Some(post) = &self.viewed else { return };
        match self.client.set_post_locked(post.id, locked) {
            Ok(post) => {
                self.message = if post.locked {
                    "Post locked"
                } else {
                    "Post unlocked"
                }
                .to_owned();
                self.viewed = Some(post);
            }
            Err(error) => self.message = error.to_string(),
        }
    }

    fn withdraw_proposal(&mut self) {
        let Some(post) = &self.viewed else { return };
        match self.client.withdraw_proposal(post.id) {
            Ok(post) => {
                self.viewed = Some(post);
                self.message = "Proposal withdrawn".to_owned();
            }
            Err(error) => self.message = error.to_string(),
        }
    }

    fn cast_vote(&mut self, selected: usize) {
        let Some(post) = self.viewed.as_ref() else {
            return;
        };
        let Some(option) = post
            .poll
            .as_ref()
            .and_then(|poll| poll.options.get(selected))
        else {
            return;
        };
        match self.client.vote(post.id, option.id) {
            Ok(post) => {
                self.viewed = Some(post);
                self.message = "Vote recorded; voting again changes your choice".to_owned();
            }
            Err(error) => self.message = error.to_string(),
        }
    }

    fn delete_selected(&mut self) {
        let Some(id) = self.selected_post().map(|post| post.id) else {
            return;
        };
        match self.client.delete_post(id) {
            Ok(()) => {
                self.message = "Post deleted".to_owned();
                self.refresh();
            }
            Err(error) => self.message = error.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn post_editor() -> Editor {
        Editor::new(
            EditorTarget::NewPost,
            "general".to_owned(),
            "Tea tiem".to_owned(),
            "First line\nSecond line".to_owned(),
            false,
        )
    }

    #[test]
    fn inserts_and_deletes_at_the_cursor() {
        let mut editor = post_editor();
        editor.title_cursor = "Tea ti".len();
        edit_key(
            &mut editor,
            KeyEvent::new(KeyCode::Delete, KeyModifiers::NONE),
        );
        edit_key(&mut editor, KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
        edit_key(
            &mut editor,
            KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE),
        );

        assert_eq!(editor.title, "Tea time");
        assert_eq!(editor.title_cursor, "Tea time".len());
    }

    #[test]
    fn moves_across_lines_and_preserves_utf8_boundaries() {
        let mut editor = post_editor();
        editor.body = "café\ntea\nbiscuits".to_owned();
        editor.body_cursor = "café\nte".len();
        editor.field = EditorField::Body;

        edit_key(&mut editor, KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(&editor.body[..editor.body_cursor], "ca");
        edit_key(
            &mut editor,
            KeyEvent::new(KeyCode::Left, KeyModifiers::NONE),
        );
        edit_key(
            &mut editor,
            KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE),
        );

        assert_eq!(editor.body, "afé\ntea\nbiscuits");
        assert_eq!(editor.body_cursor, 0);
    }

    #[test]
    fn home_end_and_enter_edit_the_active_line() {
        let mut editor = post_editor();
        editor.field = EditorField::Body;
        editor.body_cursor = "First line\nSec".len();

        edit_key(
            &mut editor,
            KeyEvent::new(KeyCode::Home, KeyModifiers::NONE),
        );
        edit_key(
            &mut editor,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        );
        edit_key(&mut editor, KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
        edit_key(
            &mut editor,
            KeyEvent::new(KeyCode::Char('!'), KeyModifiers::NONE),
        );

        assert_eq!(editor.body, "First line\n\nSecond line!");
    }
}
