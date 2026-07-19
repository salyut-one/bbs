use std::{
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixStream,
    path::PathBuf,
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};

use crate::protocol::{Board, ErrorCode, Post, PostSummary, Request, Response};

#[derive(Clone)]
pub struct Client {
    socket: PathBuf,
}

impl Client {
    pub fn new(socket: impl Into<PathBuf>) -> Self {
        Self {
            socket: socket.into(),
        }
    }

    pub fn handle(&self) -> Result<String> {
        match self.call(&Request::WhoAmI)? {
            Response::Identity { handle } => Ok(handle),
            response => unexpected(response),
        }
    }

    pub fn boards(&self) -> Result<Vec<Board>> {
        match self.call(&Request::ListBoards)? {
            Response::Boards(boards) => Ok(boards),
            response => result_error(response),
        }
    }

    pub fn posts(&self, board: &str, limit: u32, offset: u32) -> Result<Vec<PostSummary>> {
        match self.call(&Request::ListPosts {
            board: board.to_owned(),
            limit,
            offset,
        })? {
            Response::Posts(posts) => Ok(posts),
            response => result_error(response),
        }
    }

    pub fn post(&self, id: i64) -> Result<Option<Post>> {
        match self.call(&Request::GetPost { id })? {
            Response::Post(post) => Ok(Some(*post)),
            Response::Error {
                code: ErrorCode::NotFound,
                ..
            } => Ok(None),
            response => result_error(response),
        }
    }

    pub fn create_post(&self, board: &str, title: &str, body: &str) -> Result<Post> {
        post_result(self.call(&Request::CreatePost {
            board: board.to_owned(),
            title: title.to_owned(),
            body: body.to_owned(),
        })?)
    }

    pub fn create_proposal(&self, board: &str, title: &str, body: &str) -> Result<Post> {
        post_result(self.call(&Request::CreateProposal {
            board: board.to_owned(),
            title: title.to_owned(),
            body: body.to_owned(),
        })?)
    }

    pub fn update_post(&self, id: i64, title: &str, body: &str) -> Result<Post> {
        post_result(self.call(&Request::UpdatePost {
            id,
            title: title.to_owned(),
            body: body.to_owned(),
        })?)
    }

    pub fn delete_post(&self, id: i64) -> Result<()> {
        match self.call(&Request::DeletePost { id })? {
            Response::Deleted { .. } => Ok(()),
            response => result_error(response),
        }
    }

    pub fn vote(&self, post_id: i64, option_id: i64) -> Result<Post> {
        post_result(self.call(&Request::CastVote { post_id, option_id })?)
    }

    pub fn create_reply(&self, post_id: i64, body: &str) -> Result<Post> {
        post_result(self.call(&Request::CreateReply {
            post_id,
            body: body.to_owned(),
        })?)
    }

    pub fn update_reply(&self, id: i64, body: &str) -> Result<Post> {
        post_result(self.call(&Request::UpdateReply {
            id,
            body: body.to_owned(),
        })?)
    }

    pub fn delete_reply(&self, id: i64) -> Result<i64> {
        match self.call(&Request::DeleteReply { id })? {
            Response::ReplyDeleted { post_id, .. } => Ok(post_id),
            response => result_error(response),
        }
    }

    pub fn set_post_locked(&self, id: i64, locked: bool) -> Result<Post> {
        post_result(self.call(&Request::SetPostLocked { id, locked })?)
    }

    pub fn withdraw_proposal(&self, id: i64) -> Result<Post> {
        post_result(self.call(&Request::WithdrawProposal { id })?)
    }

    pub fn veto_proposal(&self, id: i64, reason: &str) -> Result<Post> {
        post_result(self.call(&Request::VetoProposal {
            id,
            reason: reason.to_owned(),
        })?)
    }

    pub fn mark_proposal_implemented(&self, id: i64, note: &str) -> Result<Post> {
        post_result(self.call(&Request::MarkProposalImplemented {
            id,
            note: note.to_owned(),
        })?)
    }

    fn call(&self, request: &Request) -> Result<Response> {
        let mut stream = UnixStream::connect(&self.socket)
            .with_context(|| format!("connect {}", self.socket.display()))?;
        let timeout = Some(Duration::from_secs(10));
        stream.set_read_timeout(timeout)?;
        stream.set_write_timeout(timeout)?;
        serde_json::to_writer(&mut stream, request)?;
        stream.write_all(b"\n")?;

        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line)?;
        if line.is_empty() {
            bail!("daemon closed the connection");
        }
        serde_json::from_str(&line).context("invalid response from daemon")
    }
}

fn post_result(response: Response) -> Result<Post> {
    match response {
        Response::Post(post) => Ok(*post),
        response => result_error(response),
    }
}

fn result_error<T>(response: Response) -> Result<T> {
    match response {
        Response::Error { message, .. } => Err(anyhow!(message)),
        response => unexpected(response),
    }
}

fn unexpected<T>(response: Response) -> Result<T> {
    Err(anyhow!("unexpected daemon response: {response:?}"))
}
