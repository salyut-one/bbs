use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum Request {
    WhoAmI,
    ListBoards,
    ListPosts {
        board: String,
        limit: u32,
        offset: u32,
    },
    GetPost {
        id: i64,
    },
    CreatePost {
        board: String,
        title: String,
        body: String,
    },
    CreateProposal {
        board: String,
        title: String,
        body: String,
    },
    UpdatePost {
        id: i64,
        title: String,
        body: String,
    },
    DeletePost {
        id: i64,
    },
    CastVote {
        post_id: i64,
        option_id: i64,
    },
    CreateReply {
        post_id: i64,
        body: String,
    },
    UpdateReply {
        id: i64,
        body: String,
    },
    DeleteReply {
        id: i64,
    },
    SetPostLocked {
        id: i64,
        locked: bool,
    },
    WithdrawProposal {
        id: i64,
    },
    VetoProposal {
        id: i64,
        reason: String,
    },
    MarkProposalImplemented {
        id: i64,
        note: String,
    },
    GetMailSubscription {
        board: String,
    },
    SetMailSubscription {
        board: String,
        subscribed: bool,
    },
    MailClaimDelivery,
    MailCompleteDelivery {
        id: i64,
    },
    MailFailDelivery {
        id: i64,
        error: String,
    },
    MailImportReply {
        token: String,
        message_id: String,
        body: String,
    },
    MailUnsubscribe {
        token: String,
    },
}

impl Request {
    pub fn is_mutating(&self) -> bool {
        matches!(
            self,
            Self::CreatePost { .. }
                | Self::CreateProposal { .. }
                | Self::UpdatePost { .. }
                | Self::DeletePost { .. }
                | Self::CastVote { .. }
                | Self::CreateReply { .. }
                | Self::UpdateReply { .. }
                | Self::DeleteReply { .. }
                | Self::SetPostLocked { .. }
                | Self::WithdrawProposal { .. }
                | Self::VetoProposal { .. }
                | Self::MarkProposalImplemented { .. }
                | Self::SetMailSubscription { .. }
                | Self::MailClaimDelivery
                | Self::MailCompleteDelivery { .. }
                | Self::MailFailDelivery { .. }
                | Self::MailImportReply { .. }
                | Self::MailUnsubscribe { .. }
        )
    }

    pub fn is_mail_worker(&self) -> bool {
        matches!(
            self,
            Self::MailClaimDelivery
                | Self::MailCompleteDelivery { .. }
                | Self::MailFailDelivery { .. }
                | Self::MailImportReply { .. }
                | Self::MailUnsubscribe { .. }
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BoardKind {
    Discussion,
    Polls,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Board {
    pub id: i64,
    pub slug: String,
    pub name: String,
    pub description: String,
    pub kind: BoardKind,
    pub write_group: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Post {
    pub id: i64,
    pub board: Board,
    pub author: String,
    pub title: String,
    pub body: String,
    pub locked: bool,
    pub replies: Vec<Reply>,
    pub poll: Option<Poll>,
    pub proposal: Option<Proposal>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProposalState {
    Voting,
    Accepted,
    Rejected,
    Withdrawn,
    Vetoed,
    Implemented,
}

impl ProposalState {
    pub fn label(self) -> &'static str {
        match self {
            Self::Voting => "voting",
            Self::Accepted => "accepted",
            Self::Rejected => "rejected",
            Self::Withdrawn => "withdrawn",
            Self::Vetoed => "vetoed",
            Self::Implemented => "implemented",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Proposal {
    pub state: ProposalState,
    pub opens_at: DateTime<Utc>,
    pub closes_at: DateTime<Utc>,
    pub closed_at: Option<DateTime<Utc>>,
    pub events: Vec<ProposalEvent>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProposalEvent {
    pub id: i64,
    pub from_state: Option<ProposalState>,
    pub to_state: ProposalState,
    pub actor_uid: Option<u32>,
    pub actor: Option<String>,
    pub reason: Option<String>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Reply {
    pub id: i64,
    pub author: String,
    pub body: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Poll {
    pub options: Vec<PollOption>,
    pub total_votes: u32,
    pub my_vote: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PollOption {
    pub id: i64,
    pub label: String,
    pub votes: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MailDelivery {
    pub id: i64,
    pub recipient: String,
    pub board_slug: String,
    pub post_id: i64,
    pub author: String,
    pub subject: String,
    pub body: String,
    pub message_id: String,
    pub in_reply_to: Option<String>,
    pub reply_token: String,
    pub unsubscribe_token: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PostSummary {
    pub id: i64,
    pub board_slug: String,
    pub author: String,
    pub title: String,
    pub is_poll: bool,
    pub proposal_state: Option<ProposalState>,
    pub locked: bool,
    pub reply_count: u32,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", content = "data", rename_all = "snake_case")]
pub enum Response {
    Identity {
        handle: String,
    },
    Boards(Vec<Board>),
    Posts(Vec<PostSummary>),
    Post(Box<Post>),
    ReplyDeleted {
        id: i64,
        post_id: i64,
    },
    Deleted {
        id: i64,
    },
    MailSubscription {
        board: String,
        subscribed: bool,
        eligible: bool,
    },
    MailDelivery(Option<Box<MailDelivery>>),
    MailDeliveryUpdated {
        id: i64,
    },
    MailReplyAccepted {
        post_id: i64,
        duplicate: bool,
    },
    MailUnsubscribed {
        board: String,
    },
    Error {
        code: ErrorCode,
        message: String,
    },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    BadRequest,
    Forbidden,
    NotFound,
}
