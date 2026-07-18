use std::{
    env,
    fs::{self, File, OpenOptions},
    io::{self, Write},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    process::Command,
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use clap::Parser;
use crossterm::event::{self, Event, KeyCode, KeyEvent};
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
    fn returns_to_view(&self) -> bool {
        matches!(
            self,
            Self::NewReply(_)
                | Self::EditReply { .. }
                | Self::VetoProposal(_)
                | Self::ImplementProposal(_)
        )
    }

    fn has_title(&self) -> bool {
        matches!(self, Self::NewPost | Self::EditPost(_))
    }
}

enum EditResult {
    Saved,
    Cancelled,
}

static TEMP_FILE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

struct TempFile {
    path: PathBuf,
}

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
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
                return edit(
                    app,
                    Editor {
                        target: EditorTarget::NewPost,
                        board_slug: board.slug.clone(),
                        title: String::new(),
                        body: String::new(),
                        creates_proposal: board.kind == BoardKind::Polls,
                    },
                );
            }
        }
        KeyCode::Char('e') => {
            if let Some(post) = app.load_selected() {
                if post.proposal.is_some() {
                    app.message = "Proposals cannot be edited after voting begins".to_owned();
                    return Mode::Browse;
                }
                if post.author == app.handle {
                    return edit(
                        app,
                        Editor {
                            target: EditorTarget::EditPost(post.id),
                            board_slug: post.board.slug,
                            title: post.title,
                            body: post.body,
                            creates_proposal: false,
                        },
                    );
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
                edit(
                    app,
                    Editor {
                        target: EditorTarget::EditPost(post.id),
                        board_slug: post.board.slug.clone(),
                        title: post.title.clone(),
                        body: post.body.clone(),
                        creates_proposal: false,
                    },
                )
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
                return edit(
                    app,
                    Editor {
                        target: EditorTarget::NewReply(post.id),
                        board_slug: post.board.slug.clone(),
                        title: String::new(),
                        body: String::new(),
                        creates_proposal: false,
                    },
                );
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
            edit(
                app,
                Editor {
                    target: EditorTarget::EditReply { id: reply.id },
                    board_slug: post.board.slug.clone(),
                    title: String::new(),
                    body: reply.body.clone(),
                    creates_proposal: false,
                },
            )
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
            edit(
                app,
                Editor {
                    target,
                    board_slug: post.board.slug.clone(),
                    title: String::new(),
                    body: String::new(),
                    creates_proposal: false,
                },
            )
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

fn edit(app: &mut App, mut editor: Editor) -> Mode {
    let destination = if editor.target.returns_to_view() {
        Mode::View
    } else {
        Mode::Browse
    };
    match edit_externally(&mut editor) {
        Ok(EditResult::Saved) => {
            app.save(&editor);
        }
        Ok(EditResult::Cancelled) => {
            app.message = "Edit cancelled".to_owned();
        }
        Err(error) => {
            app.message = format!("Could not complete edit: {error:#}");
        }
    }
    destination
}

fn edit_externally(editor: &mut Editor) -> Result<EditResult> {
    let (temporary, mut file) = create_temp_file()?;
    file.write_all(render_editor_document(editor).as_bytes())
        .context("write editor file")?;
    file.flush().context("flush editor file")?;
    drop(file);

    if let Err(error) = suspend_terminal() {
        let _ = resume_terminal();
        return Err(error.context("suspend terminal for editor"));
    }
    let editor_result = run_editor(&temporary.path);
    let resume_result = resume_terminal();
    if let Err(error) = resume_result {
        return Err(error.context("restore terminal after editor"));
    }
    let status = editor_result?;
    if !status.success() {
        return Ok(EditResult::Cancelled);
    }

    let document = fs::read_to_string(&temporary.path).context("read editor file")?;
    parse_editor_document(editor, &document)?;
    Ok(EditResult::Saved)
}

fn create_temp_file() -> Result<(TempFile, File)> {
    let directory = env::temp_dir();
    for _ in 0..100 {
        let sequence = TEMP_FILE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = directory.join(format!("salyut-bbs-{}-{sequence}.txt", std::process::id()));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
        {
            Ok(file) => return Ok((TempFile { path }, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error).context("create editor file"),
        }
    }
    bail!("could not allocate a temporary editor file")
}

fn suspend_terminal() -> Result<()> {
    crossterm::terminal::disable_raw_mode()?;
    crossterm::execute!(
        io::stdout(),
        crossterm::terminal::LeaveAlternateScreen,
        crossterm::cursor::Show
    )?;
    Ok(())
}

fn resume_terminal() -> Result<()> {
    crossterm::execute!(
        io::stdout(),
        crossterm::terminal::EnterAlternateScreen,
        crossterm::cursor::Hide
    )?;
    crossterm::terminal::enable_raw_mode()?;
    Ok(())
}

fn run_editor(path: &Path) -> Result<std::process::ExitStatus> {
    let editor = env::var("EDITOR")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "vi".to_owned());
    Command::new("/bin/sh")
        .args(["-c", "exec $EDITOR \"$1\"", "salyut-bbs"])
        .arg(path)
        .env("EDITOR", editor)
        .status()
        .context("start $EDITOR")
}

fn render_editor_document(editor: &Editor) -> String {
    if editor.target.has_title() {
        format!("Title: {}\n\n{}", editor.title, editor.body)
    } else {
        editor.body.clone()
    }
}

fn parse_editor_document(editor: &mut Editor, document: &str) -> Result<()> {
    let normalized = document.replace("\r\n", "\n");
    if editor.target.has_title() {
        let (header, remainder) = normalized
            .split_once('\n')
            .ok_or_else(|| anyhow!("expected a `Title:` line followed by a blank line"))?;
        let title = header
            .strip_prefix("Title:")
            .ok_or_else(|| anyhow!("the first line must start with `Title:`"))?
            .trim();
        let body = remainder
            .strip_prefix('\n')
            .ok_or_else(|| anyhow!("expected a blank line after the title"))?;
        editor.title = title.to_owned();
        editor.body = body.trim_end_matches('\n').to_owned();
    } else {
        editor.body = normalized.trim_end_matches('\n').to_owned();
    }
    Ok(())
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
        Editor {
            target: EditorTarget::NewPost,
            board_slug: "general".to_owned(),
            title: "Initial title".to_owned(),
            body: "Initial body".to_owned(),
            creates_proposal: false,
        }
    }

    #[test]
    fn post_document_round_trips_title_and_body() {
        let mut editor = post_editor();
        parse_editor_document(
            &mut editor,
            "Title: A clearer title\n\nFirst line\nSecond line\n",
        )
        .unwrap();

        assert_eq!(editor.title, "A clearer title");
        assert_eq!(editor.body, "First line\nSecond line");
        assert_eq!(
            render_editor_document(&editor),
            "Title: A clearer title\n\nFirst line\nSecond line"
        );
    }

    #[test]
    fn post_document_requires_title_header_and_separator() {
        let mut editor = post_editor();
        assert!(parse_editor_document(&mut editor, "A title\n\nBody").is_err());
        assert!(parse_editor_document(&mut editor, "Title: A title\nBody").is_err());
    }

    #[test]
    fn reply_document_is_body_only() {
        let mut editor = Editor {
            target: EditorTarget::NewReply(7),
            board_slug: "general".to_owned(),
            title: String::new(),
            body: String::new(),
            creates_proposal: false,
        };
        parse_editor_document(&mut editor, "A reply\nwith two lines\n").unwrap();

        assert_eq!(editor.body, "A reply\nwith two lines");
        assert_eq!(render_editor_document(&editor), "A reply\nwith two lines");
    }
}
