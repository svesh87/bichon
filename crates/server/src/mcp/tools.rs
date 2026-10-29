//
// Copyright (c) 2025-2026 rustmailer.com (https://rustmailer.com)
//
// This file is part of the Bichon Email Archiving Project
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU Affero General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU Affero General Public License for more details.
//
// You should have received a copy of the GNU Affero General Public License
// along with this program.  If not, see <http://www.gnu.org/licenses/>.

use std::{collections::HashSet, time::Duration};

use bichon_core::{
    account::{
        access::{load_readable_with_access, search_scope},
        migration::{AccountModel, AccountType},
    },
    archive::imap::{mailbox::MailBox, task::SYNC_TASKS},
    common::auth::ClientContext,
    message::{
        attachment::retrieve_attachment_content,
        content::retrieve_email_content,
        list::get_thread_messages,
        search::{search_messages_scoped, EmailSearchFilter, EmailSearchRequest},
    },
    store::envelope::Envelope,
    users::permissions::Permission,
};
use chrono::{DateTime, NaiveDate, TimeZone, Utc};
use poem_mcpserver::{content::Json, Tools};
use serde::Serialize;

use super::{
    draft::{save_draft, DraftRequest},
    files,
};

/// Tools for AI agents over the Bichon archive: find and read mail, pull new
/// mail now, export attachments, save drafts. Nothing here sends, deletes or
/// moves mail.
pub struct BichonMcpTools {
    context: ClientContext,
}

/// An envelope trimmed to what an agent needs to pick a message.
#[derive(Serialize, Debug)]
pub struct EnvelopeOut {
    account_id: u64,
    envelope_id: String,
    thread_id: String,
    folder: Option<String>,
    date: String,
    from: String,
    to: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    cc: Vec<String>,
    subject: String,
    preview: String,
    attachments: usize,
}

impl From<Envelope> for EnvelopeOut {
    fn from(e: Envelope) -> Self {
        Self {
            account_id: e.account_id,
            envelope_id: e.id,
            thread_id: e.thread_id,
            folder: e.mailbox_name,
            date: format_ms(e.date),
            from: e.from,
            to: e.to,
            cc: e.cc,
            subject: e.subject,
            preview: e.preview,
            attachments: e.regular_attachment_count,
        }
    }
}

#[derive(Serialize, Debug)]
pub struct PageOut {
    total: u64,
    page: u64,
    messages: Vec<EnvelopeOut>,
}

#[derive(Serialize, Debug)]
pub struct AttachmentOut {
    filename: Option<String>,
    content_type: String,
    size: usize,
    content_hash: String,
}

#[derive(Serialize, Debug)]
pub struct EmailOut {
    #[serde(flatten)]
    envelope: EnvelopeOut,
    message_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    html: Option<String>,
    attachments: Vec<AttachmentOut>,
}

#[derive(Serialize, Debug)]
pub struct SavedFile {
    filename: String,
    path: String,
    size: usize,
}

pub(super) fn format_ms(ms: i64) -> String {
    Utc.timestamp_millis_opt(ms)
        .single()
        .map(|d| d.to_rfc3339())
        .unwrap_or_default()
}

/// `YYYY-MM-DD` (UTC midnight) or RFC 3339 → Unix milliseconds.
pub(super) fn parse_date(value: &str) -> Result<i64, String> {
    let value = value.trim();
    if let Ok(d) = DateTime::parse_from_rfc3339(value) {
        return Ok(d.timestamp_millis());
    }
    NaiveDate::parse_from_str(value, "%Y-%m-%d")
        .map(|d| d.and_hms_opt(0, 0, 0).unwrap().and_utc().timestamp_millis())
        .map_err(|_| format!("Bad date {value:?}: use YYYY-MM-DD or RFC 3339"))
}

fn page_out(page: bichon_core::common::paginated::DataPage<Envelope>, number: u64) -> PageOut {
    PageOut {
        total: page.total_items,
        page: number,
        messages: page.items.into_iter().map(EnvelopeOut::from).collect(),
    }
}

impl BichonMcpTools {
    pub fn new(context: ClientContext) -> Self {
        Self { context }
    }

    fn require(&self, account_id: u64, permission: &str) -> Result<(), String> {
        self.context
            .require_permission(Some(account_id), permission)
            .map_err(|e| format!("Permission denied: {e:#}"))
    }

    fn readable(&self, account_id: u64, envelope_id: &str) -> Result<Envelope, String> {
        load_readable_with_access(&self.context, account_id, envelope_id)
            .map(|(envelope, _)| envelope)
            .map_err(|e| format!("Message {envelope_id}: {e:#}"))
    }
}

#[Tools]
impl BichonMcpTools {
    /// List the mail accounts (mailboxes) available to you, with their IDs.
    pub async fn list_accounts(&self) -> Result<Json<Vec<serde_json::Value>>, String> {
        let accounts = AccountModel::minimal_list(false)
            .map_err(|e| format!("Failed to list accounts: {e:#}"))?;
        Ok(Json(
            accounts
                .into_iter()
                .filter(|a| {
                    self.context
                        .has_permission(Some(a.id), Permission::DATA_READ)
                })
                .map(|a| serde_json::json!({"account_id": a.id, "email": a.email, "name": a.name}))
                .collect(),
        ))
    }

    /// List the folders of an account as archived: name, message count, IDs for
    /// `search_emails.folder_ids`.
    pub async fn list_folders(
        &self,
        /// Account ID from list_accounts
        account_id: u64,
    ) -> Result<Json<Vec<serde_json::Value>>, String> {
        self.require(account_id, Permission::DATA_READ)?;
        let folders =
            MailBox::list_all(account_id).map_err(|e| format!("Failed to list folders: {e:#}"))?;
        Ok(Json(
            folders
                .into_iter()
                .map(|m| serde_json::json!({"folder_id": m.id, "name": m.name, "messages": m.exists}))
                .collect(),
        ))
    }

    /// Pull new mail of an account from the mail server now and wait until it is
    /// archived. Call before searching when the newest mail matters (codes,
    /// replies that just arrived).
    pub async fn sync_now(
        &self,
        /// Account ID from list_accounts
        account_id: u64,
        /// How long to wait for the sync, seconds (default 120, max 600)
        wait_seconds: Option<u64>,
    ) -> Result<Json<serde_json::Value>, String> {
        if !self
            .context
            .has_permission(Some(account_id), Permission::ACCOUNT_SYNC)
        {
            self.require(account_id, Permission::ACCOUNT_MANAGE)?;
        }
        let account = AccountModel::check_account_exists(account_id)
            .map_err(|e| format!("Account {account_id}: {e:#}"))?;
        if !matches!(account.account_type, AccountType::IMAP) {
            return Err("Only IMAP accounts can be synced".into());
        }
        let wait = Duration::from_secs(wait_seconds.unwrap_or(120).clamp(5, 600));
        let started = tokio::time::Instant::now();
        let deadline = started + wait;

        // A scheduled sync may be running: wait for it to end, then start ours
        // (incremental, so it only fetches what arrived meanwhile).
        loop {
            match SYNC_TASKS.start_manual_task(account_id, false).await {
                Ok(()) => break,
                Err(e) if tokio::time::Instant::now() >= deadline => {
                    return Err(format!(
                        "Sync did not start within {}s: {e:#}",
                        wait.as_secs()
                    ));
                }
                Err(_) => tokio::time::sleep(Duration::from_secs(2)).await,
            }
        }
        while SYNC_TASKS.is_manual_running(account_id).await {
            if tokio::time::Instant::now() >= deadline {
                return Ok(Json(serde_json::json!({
                    "account": account.email,
                    "status": "still running",
                    "elapsed_seconds": started.elapsed().as_secs(),
                })));
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        Ok(Json(serde_json::json!({
            "account": account.email,
            "status": "finished",
            "elapsed_seconds": started.elapsed().as_secs(),
        })))
    }

    /// Search archived mail (all folders, everything ever downloaded, including
    /// mail deleted on the server since). Newest first, or by relevance when
    /// `text` is given.
    // Each argument is a field of the MCP tool's input schema.
    #[allow(clippy::too_many_arguments)]
    pub async fn search_emails(
        &self,
        /// Full-text query over subject, body, addresses
        text: Option<String>,
        /// Words in the subject
        subject: Option<String>,
        /// Sender address or name
        from: Option<String>,
        /// Recipient address (To)
        to: Option<String>,
        /// Address in From, To, Cc or Bcc
        participant: Option<String>,
        /// Sent on or after this date (YYYY-MM-DD or RFC 3339)
        since: Option<String>,
        /// Sent before this date (YYYY-MM-DD or RFC 3339)
        before: Option<String>,
        /// Limit to these accounts (IDs from list_accounts)
        account_ids: Option<Vec<u64>>,
        /// Limit to these folders (IDs from list_folders)
        folder_ids: Option<Vec<u64>>,
        /// Only messages with (true) or without (false) attachments
        has_attachment: Option<bool>,
        /// Words in an attachment file name
        attachment_name: Option<String>,
        /// Page number, from 1 (default 1)
        page: Option<u64>,
        /// Messages per page (default 20, max 100)
        page_size: Option<u64>,
    ) -> Result<Json<PageOut>, String> {
        let filter = EmailSearchFilter {
            text,
            subject,
            from,
            to,
            any_participant: participant,
            since: since.as_deref().map(parse_date).transpose()?,
            before: before.as_deref().map(parse_date).transpose()?,
            account_ids: account_ids.map(|ids| ids.into_iter().collect::<HashSet<_>>()),
            mailbox_ids: folder_ids.map(|ids| ids.into_iter().collect()),
            has_attachment,
            attachment_name,
            ..Default::default()
        };
        let scope = search_scope(&self.context, filter.account_ids.as_ref())
            .map_err(|e| format!("Permission denied: {e:#}"))?;
        let page = page.unwrap_or(1).max(1);
        let request = EmailSearchRequest {
            filter,
            page,
            page_size: page_size.unwrap_or(20).clamp(1, 100),
            sort_by: None,
            desc: Some(true),
        };
        search_messages_scoped(scope, request)
            .map(|result| Json(page_out(result, page)))
            .map_err(|e| format!("Search failed: {e:#}"))
    }

    /// Read one message: headers, plain-text body and the list of attachments
    /// (use save_attachments to get the files).
    pub async fn get_email(
        &self,
        /// Account ID
        account_id: u64,
        /// envelope_id from search_emails or get_thread
        envelope_id: String,
        /// Also return the HTML body (remote content stripped); default false.
        /// HTML-only mail always has `text` converted from the HTML.
        include_html: Option<bool>,
    ) -> Result<Json<EmailOut>, String> {
        let envelope_id = envelope_id.trim().to_string();
        let envelope = self.readable(account_id, &envelope_id)?;
        let content = retrieve_email_content(account_id, envelope_id.clone(), true)
            .map_err(|e| format!("Failed to read message: {e:#}"))?;
        let want_html = include_html.unwrap_or(false) || content.text.is_none();
        let message_id = envelope.message_id.clone();
        Ok(Json(EmailOut {
            envelope: envelope.into(),
            message_id,
            text: content.text,
            html: if want_html { content.html } else { None },
            attachments: content
                .attachments
                .unwrap_or_default()
                .into_iter()
                .map(|a| AttachmentOut {
                    filename: a.filename,
                    content_type: a.file_type,
                    size: a.size,
                    content_hash: a.content_hash,
                })
                .collect(),
        }))
    }

    /// List the messages of a conversation (thread), oldest to newest page by page.
    pub async fn get_thread(
        &self,
        /// Account ID
        account_id: u64,
        /// thread_id from search_emails or get_email
        thread_id: String,
        /// Page number, from 1 (default 1)
        page: Option<u64>,
        /// Messages per page (default 50, max 100)
        page_size: Option<u64>,
    ) -> Result<Json<PageOut>, String> {
        self.require(account_id, Permission::DATA_READ)?;
        let page = page.unwrap_or(1).max(1);
        get_thread_messages(
            account_id,
            thread_id.trim(),
            page,
            page_size.unwrap_or(50).clamp(1, 100),
        )
        .map(|result| Json(page_out(result, page)))
        .map_err(|e| format!("Failed to read thread: {e:#}"))
    }

    /// Save attachments of a message as files in the shared file directory and
    /// return their paths. Without content_hash, saves all regular attachments.
    pub async fn save_attachments(
        &self,
        /// Account ID
        account_id: u64,
        /// envelope_id of the message
        envelope_id: String,
        /// content_hash of one attachment (from get_email); omit for all
        content_hash: Option<String>,
    ) -> Result<Json<Vec<SavedFile>>, String> {
        let envelope_id = envelope_id.trim().to_string();
        let envelope = self.readable(account_id, &envelope_id)?;
        let content = retrieve_email_content(account_id, envelope_id.clone(), true)
            .map_err(|e| format!("Failed to read message: {e:#}"))?;
        let wanted: Vec<_> = content
            .attachments
            .unwrap_or_default()
            .into_iter()
            .filter(|a| match &content_hash {
                Some(hash) => a.content_hash == hash.trim(),
                None => !a.inline,
            })
            .collect();
        if wanted.is_empty() {
            return Err("No such attachment in this message".into());
        }
        let account = envelope
            .account_email
            .clone()
            .unwrap_or_else(|| account_id.to_string());
        let day = format_ms(envelope.date)
            .chars()
            .take(10)
            .collect::<String>();
        let folder = format!(
            "{day} {}",
            envelope.subject.chars().take(60).collect::<String>()
        );
        let mut saved = Vec::new();
        for attachment in wanted {
            let mut reader = retrieve_attachment_content(
                account_id,
                envelope_id.clone(),
                &attachment.content_hash,
            )
            .map_err(|e| format!("Failed to read attachment: {e:#}"))?;
            let mut bytes = Vec::new();
            std::io::Read::read_to_end(&mut reader, &mut bytes)
                .map_err(|e| format!("Failed to read attachment: {e}"))?;
            let name = attachment
                .filename
                .clone()
                .unwrap_or_else(|| attachment.content_hash.clone());
            let path = files::save(&["attachments", &account, &folder], &name, &bytes)?;
            saved.push(SavedFile {
                filename: name,
                path: path.display().to_string(),
                size: bytes.len(),
            });
        }
        Ok(Json(saved))
    }

    /// Save a draft into the account's Drafts folder on the mail server (it then
    /// shows up in the provider's web mail). Never sends. For a reply give
    /// reply_to_envelope_id: threading headers are set, and `to`/`subject`
    /// default to the original sender and "Re: <subject>".
    #[allow(clippy::too_many_arguments)]
    pub async fn save_draft(
        &self,
        /// Account ID to save the draft in (its address is the sender)
        account_id: u64,
        /// Recipient addresses (may be empty for a reply)
        to: Option<Vec<String>>,
        /// Cc addresses
        cc: Option<Vec<String>>,
        /// Bcc addresses
        bcc: Option<Vec<String>>,
        /// Subject (default for a reply: "Re: <original subject>")
        subject: Option<String>,
        /// Plain-text body
        text: Option<String>,
        /// HTML body
        html: Option<String>,
        /// envelope_id of the message this draft replies to
        reply_to_envelope_id: Option<String>,
        /// Paths of files to attach; only files in the shared file directory
        attachments: Option<Vec<String>>,
        /// Folder name from list_folders, if the server does not mark a Drafts folder
        folder: Option<String>,
    ) -> Result<Json<serde_json::Value>, String> {
        self.require(account_id, Permission::DATA_DRAFT)?;
        if let Some(id) = &reply_to_envelope_id {
            self.readable(account_id, id.trim())?;
        }
        let saved = save_draft(DraftRequest {
            account_id,
            to: to.unwrap_or_default(),
            cc: cc.unwrap_or_default(),
            bcc: bcc.unwrap_or_default(),
            subject,
            text,
            html,
            reply_to_envelope_id: reply_to_envelope_id.map(|id| id.trim().to_string()),
            attachments: attachments.unwrap_or_default(),
            folder,
        })
        .await?;
        serde_json::to_value(saved)
            .map(Json)
            .map_err(|e| e.to_string())
    }
}
