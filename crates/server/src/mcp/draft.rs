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

//! Drafts: build a message and APPEND it to the account's Drafts folder on
//! the IMAP server. Nothing is ever sent; the owner sends from their client.

use bichon_core::{
    account::migration::{AccountModel, AccountType},
    archive::imap::mailbox::{Attribute, AttributeEnum, MailBox},
    envelope::extractor::reattach_eml_content,
    imap::executor::ImapExecutor,
};
use mail_parser::{HeaderValue, MessageParser};
use mail_send::mail_builder::{headers::address::Address, MessageBuilder};
use serde::Serialize;

use super::files::{resolve_input, MAX_DRAFT_ATTACHMENTS_BYTES};

#[derive(Default)]
pub struct DraftRequest {
    pub account_id: u64,
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub bcc: Vec<String>,
    pub subject: Option<String>,
    pub text: Option<String>,
    pub html: Option<String>,
    pub reply_to_envelope_id: Option<String>,
    pub attachments: Vec<String>,
    pub folder: Option<String>,
}

#[derive(Serialize, Debug)]
pub struct DraftSaved {
    pub account: String,
    pub folder: String,
    pub message_id: String,
    pub subject: String,
    pub to: Vec<String>,
    pub attachments: Vec<String>,
    pub size: usize,
}

/// What a reply takes from the original message.
#[derive(Debug, Default)]
pub struct ReplyContext {
    pub message_id: Option<String>,
    pub references: Vec<String>,
    pub subject: String,
    pub reply_to: Vec<String>,
}

/// A file to attach: MIME type, file name, content.
pub type DraftFile = (String, String, Vec<u8>);

/// A built draft, ready to be appended.
#[derive(Debug)]
pub struct BuiltDraft {
    pub raw: Vec<u8>,
    pub message_id: String,
    pub subject: String,
    pub to: Vec<String>,
    pub attachments: Vec<String>,
}

pub fn reply_context(eml: &[u8]) -> Result<ReplyContext, String> {
    let message = MessageParser::default()
        .parse(eml)
        .ok_or("Original message: failed to parse EML")?;
    let mut references: Vec<String> = match message.references() {
        HeaderValue::Text(id) => vec![id.to_string()],
        HeaderValue::TextList(ids) => ids.iter().map(|id| id.to_string()).collect(),
        _ => Vec::new(),
    };
    let message_id = message.message_id().map(str::to_string);
    if let Some(id) = &message_id {
        references.push(id.clone());
    }
    let reply_to = message
        .reply_to()
        .or_else(|| message.from())
        .map(|list| {
            list.iter()
                .filter_map(|addr| addr.address().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    Ok(ReplyContext {
        message_id,
        references,
        subject: message.subject().unwrap_or_default().to_string(),
        reply_to,
    })
}

pub fn reply_subject(original: &str) -> String {
    let lower = original.trim_start().to_lowercase();
    if lower.starts_with("re:") || lower.starts_with("re[") {
        original.to_string()
    } else {
        format!("Re: {original}")
    }
}

/// Read the attachments: only regular files from the file exchange directory,
/// at most `MAX_DRAFT_ATTACHMENTS_BYTES` in total.
pub fn read_attachments(paths: &[String]) -> Result<Vec<DraftFile>, String> {
    let mut files = Vec::new();
    let mut total: u64 = 0;
    for path in paths {
        let real = resolve_input(path)?;
        let content = std::fs::read(&real).map_err(|e| format!("Read {}: {e}", real.display()))?;
        total += content.len() as u64;
        if total > MAX_DRAFT_ATTACHMENTS_BYTES {
            return Err(format!(
                "Attachments exceed {} MB in total",
                MAX_DRAFT_ATTACHMENTS_BYTES / 1024 / 1024
            ));
        }
        let name = real
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "attachment".into());
        let mime = mime_guess::from_path(&real)
            .first_or_octet_stream()
            .to_string();
        files.push((mime, name, content));
    }
    Ok(files)
}

/// Build the RFC 822 draft. The sender is the account itself.
pub fn build_draft(
    account_email: &str,
    account_name: Option<&str>,
    req: &DraftRequest,
    reply: Option<&ReplyContext>,
    files: Vec<DraftFile>,
) -> Result<BuiltDraft, String> {
    let to = if req.to.is_empty() {
        reply.map(|r| r.reply_to.clone()).unwrap_or_default()
    } else {
        req.to.clone()
    };
    if to.is_empty() {
        return Err("No recipients: give `to` (or reply to a message that has a sender)".into());
    }
    let subject = match (&req.subject, reply) {
        (Some(subject), _) => subject.clone(),
        (None, Some(reply)) => reply_subject(&reply.subject),
        (None, None) => return Err("No subject".into()),
    };
    if req.text.is_none() && req.html.is_none() {
        return Err("No body: give `text` and/or `html`".into());
    }

    let domain = account_email.rsplit('@').next().unwrap_or("localhost");
    let message_id = format!("{}@{}", uuid::Uuid::new_v4(), domain);
    let from: Address = match account_name {
        Some(name) if !name.trim().is_empty() => {
            (name.to_string(), account_email.to_string()).into()
        }
        _ => account_email.to_string().into(),
    };
    let mut builder = MessageBuilder::new()
        .from(from)
        .to(to.clone())
        .subject(subject.clone())
        .message_id(message_id.clone());
    if !req.cc.is_empty() {
        builder = builder.cc(req.cc.clone());
    }
    if !req.bcc.is_empty() {
        builder = builder.bcc(req.bcc.clone());
    }
    if let Some(reply) = reply {
        if let Some(id) = &reply.message_id {
            builder = builder.in_reply_to(id.clone());
        }
        if !reply.references.is_empty() {
            builder = builder.references(reply.references.clone());
        }
    }
    if let Some(text) = &req.text {
        builder = builder.text_body(text.clone());
    }
    if let Some(html) = &req.html {
        builder = builder.html_body(html.clone());
    }
    let mut attachments = Vec::new();
    for (mime, name, content) in files {
        attachments.push(name.clone());
        builder = builder.attachment(mime, name, content);
    }
    let raw = builder
        .write_to_vec()
        .map_err(|e| format!("Build message: {e}"))?;
    Ok(BuiltDraft {
        raw,
        message_id,
        subject,
        to,
        attachments,
    })
}

/// APPEND `raw` to the Drafts folder (or `folder`, a name from list_folders)
/// with `\Draft \Seen`. Returns the folder name as shown to users.
pub async fn append_draft(
    account_id: u64,
    folder: Option<&str>,
    raw: &[u8],
) -> Result<String, String> {
    let mut session = ImapExecutor::create_connection(account_id)
        .await
        .map_err(|e| format!("IMAP connection: {e:#}"))?;
    let (folder_raw, folder_shown) = match folder {
        Some(name) => {
            let mailbox = MailBox::list_all(account_id)
                .map_err(|e| format!("Folders: {e:#}"))?
                .into_iter()
                .find(|m| m.name == name)
                .ok_or_else(|| format!("Folder {name:?} is not known for this account"))?;
            (mailbox.encoded_name(), mailbox.name)
        }
        None => {
            let names = ImapExecutor::list_all_mailboxes(&mut session)
                .await
                .map_err(|e| format!("IMAP LIST: {e:#}"))?;
            let drafts = names
                .iter()
                .find(|n| {
                    n.attributes()
                        .iter()
                        .any(|a| Attribute::from(a).attr == AttributeEnum::Drafts)
                })
                .map(|n| n.name().to_string())
                .ok_or("The server marks no folder as \\Drafts; give `folder` explicitly")?;
            let shown = MailBox::list_all(account_id)
                .ok()
                .and_then(|list| list.into_iter().find(|m| m.encoded_name() == drafts))
                .map(|m| m.name)
                .unwrap_or_else(|| drafts.clone());
            (drafts, shown)
        }
    };
    ImapExecutor::append(
        &mut session,
        &folder_raw,
        Some("(\\Draft \\Seen)"),
        None,
        raw,
    )
    .await
    .map_err(|e| format!("IMAP APPEND to {folder_shown}: {e:#}"))?;
    let _ = session.logout().await;
    Ok(folder_shown)
}

/// Build the draft and append it to the Drafts folder. Permission checks
/// (draft permission, read access to the replied message) are the caller's.
pub async fn save_draft(req: DraftRequest) -> Result<DraftSaved, String> {
    let account = AccountModel::check_account_exists(req.account_id)
        .map_err(|e| format!("Account {}: {e:#}", req.account_id))?;
    if !matches!(account.account_type, AccountType::IMAP) {
        return Err("Drafts are only supported for IMAP accounts".into());
    }
    let reply = match &req.reply_to_envelope_id {
        Some(id) => {
            let (_, eml) = reattach_eml_content(req.account_id, id.clone())
                .map_err(|e| format!("Original message: {e:#}"))?;
            Some(reply_context(&eml)?)
        }
        None => None,
    };
    let files = read_attachments(&req.attachments)?;
    let built = build_draft(
        &account.email,
        account.account_name.as_deref(),
        &req,
        reply.as_ref(),
        files,
    )?;
    let folder = append_draft(req.account_id, req.folder.as_deref(), &built.raw).await?;
    tracing::info!(
        account_id = req.account_id,
        folder = %folder,
        message_id = %built.message_id,
        "MCP draft saved"
    );
    Ok(DraftSaved {
        account: account.email,
        folder,
        message_id: built.message_id,
        subject: built.subject,
        to: built.to,
        attachments: built.attachments,
        size: built.raw.len(),
    })
}
