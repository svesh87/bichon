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

//! svesh: chunked body download, opt-in: per account (`imap_fetch_chunk_kb`,
//! the "chunk size" field of the account's download settings) or for the
//! whole server (`BICHON_IMAP_FETCH_CHUNK_KB`).
//!
//! The default download asks for the whole bodies of a batch of messages in
//! one `UID FETCH`. Some servers (Yandex) stall on such large responses and
//! drop the connection mid-transfer. In chunked mode each message is fetched
//! on its own, in parts of `BODY.PEEK[]<offset.length>`, as Thunderbird does
//! (`mail.imap.chunk_size`, 136 KB by default): small responses, steady
//! traffic, and a dropped connection costs one part instead of a batch.
//! The chunk size is the most one response may carry: small messages share
//! one FETCH while their total stays within it, so ordinary mail costs a few
//! round trips per batch, not one per message.

use std::sync::LazyLock;

use async_imap::Session;
use futures::TryStreamExt;
use tokio_util::sync::CancellationToken;

use crate::{
    account::{migration::AccountModel, state::DownloadState},
    envelope::extractor::extract_raw_and_store_it,
    error::{code::ErrorCode, BichonResult},
    imap::{
        executor::{classify_imap_error, BatchFetchOutcome},
        session::SessionStream,
    },
    raise_error,
};

/// Part size in bytes, or `None` when chunked mode is off (unset, empty or 0).
pub static CHUNK_BYTES: LazyLock<Option<u32>> =
    LazyLock::new(|| parse_chunk_kb(std::env::var("BICHON_IMAP_FETCH_CHUNK_KB").ok().as_deref()));

/// `BICHON_IMAP_FETCH_CHUNK_KB`: kilobytes per part, 4..=65536; anything else
/// (unset, 0, not a number) leaves chunked mode off.
pub fn parse_chunk_kb(value: Option<&str>) -> Option<u32> {
    let kb: u32 = value?.trim().parse().ok()?;
    if kb == 0 {
        return None;
    }
    Some(kb.clamp(4, 65_536) * 1024)
}

/// Part size for an account: its own setting (`imap_fetch_chunk_kb`; `0` =
/// off) wins over the server default.
pub fn effective_chunk(account_kb: Option<u32>, server_default: Option<u32>) -> Option<u32> {
    match account_kb {
        Some(kb) => parse_chunk_kb(Some(&kb.to_string())),
        None => server_default,
    }
}

/// [`effective_chunk`] for a stored account; an unreadable account uses the
/// server default.
pub fn chunk_for_account(account_id: u64) -> Option<u32> {
    let account_kb = AccountModel::get(account_id)
        .ok()
        .and_then(|account| account.imap_fetch_chunk_kb);
    effective_chunk(account_kb, *CHUNK_BYTES)
}

/// The fork's gentle mode for slow servers is on for this account (a chunk
/// size is set): chunked download, gap-fill by UID, skipping folders without
/// new mail, waiting before reconnects. Off (the default), every account
/// syncs exactly as upstream does.
pub fn is_gentle(account: &AccountModel) -> bool {
    effective_chunk(account.imap_fetch_chunk_kb, *CHUNK_BYTES).is_some()
}

/// The whole body of one message, fetched in parts. When the first part
/// brings no body (Yandex does that for some messages) the message is asked
/// for whole, as upstream does; `None` only if that brings no body either.
/// A part missing after the first one is an error: a truncated message is
/// never returned.
pub async fn fetch_message_in_chunks(
    session: &mut Session<Box<dyn SessionStream>>,
    uid: u32,
    size: u32,
    chunk: u32,
) -> BichonResult<Option<(Vec<u8>, i64)>> {
    if let Some(found) = fetch_parts(session, uid, size, chunk).await? {
        return Ok(Some(found));
    }
    let whole = fetch_group(session, &[(uid, size)]).await?;
    if whole.is_empty() {
        let server = probe(session, uid).await;
        tracing::warn!(
            uid,
            size,
            server,
            "No body for the message, neither in parts nor whole"
        );
    } else {
        tracing::warn!(
            uid,
            size,
            "No body for the first part; fetched whole instead"
        );
    }
    Ok(whole.into_iter().next().map(|(_, body, date)| (body, date)))
}

/// What the server says to a one-byte fetch of `uid`: the FETCH stream drops
/// the command's final status, so a refusal (`NO [...] text`) is otherwise
/// invisible. For the log only.
pub async fn probe(session: &mut Session<Box<dyn SessionStream>>, uid: u32) -> String {
    match session
        .run_command_and_check_ok(format!("UID FETCH {uid} (UID BODY.PEEK[]<0.1>)"))
        .await
    {
        Ok(()) => "OK (the server answers, but sent no body)".to_string(),
        Err(e) => format!("{e:?}"),
    }
}

async fn fetch_parts(
    session: &mut Session<Box<dyn SessionStream>>,
    uid: u32,
    size: u32,
    chunk: u32,
) -> BichonResult<Option<(Vec<u8>, i64)>> {
    let mut body: Vec<u8> = Vec::with_capacity(size as usize);
    let mut internal_date = 0i64;
    loop {
        let offset = body.len();
        let items = if offset == 0 {
            format!("(UID INTERNALDATE BODY.PEEK[]<0.{chunk}>)")
        } else {
            format!("(UID BODY.PEEK[]<{offset}.{chunk}>)")
        };
        let mut part: Option<Vec<u8>> = None;
        // What came back, for the log when no body did.
        let mut seen: Vec<String> = Vec::new();
        {
            let mut stream = session
                .uid_fetch(uid.to_string(), &items)
                .await
                .map_err(|e| raise_error!(format!("{:#?}", e), classify_imap_error(&e)))?;
            while let Some(fetch) = stream
                .try_next()
                .await
                .map_err(|e| raise_error!(format!("{:#?}", e), classify_imap_error(&e)))?
            {
                seen.push(format!(
                    "uid={:?} body={} size={:?}",
                    fetch.uid,
                    fetch.body().map_or(0, <[u8]>::len),
                    fetch.size
                ));
                // Unsolicited FETCH responses (flag changes of other messages)
                // may come along; only ours counts.
                if fetch.uid != Some(uid) {
                    continue;
                }
                if offset == 0 {
                    internal_date = fetch
                        .internal_date()
                        .map(|d| d.timestamp_millis())
                        .unwrap_or(0);
                }
                if let Some(data) = fetch.body() {
                    part = Some(data.to_vec());
                }
            }
        }
        let Some(part) = part else {
            if offset == 0 {
                tracing::warn!(uid, size, chunk, responses = ?seen, "No body in the first part");
                return Ok(None);
            }
            // Never store a truncated message: fail, the batch is retried.
            return Err(raise_error!(
                format!("UID {uid}: no data at offset {offset} of {size} bytes"),
                ErrorCode::ImapCommandFailed
            ));
        };
        let short = (part.len() as u32) < chunk;
        body.extend_from_slice(&part);
        if part.is_empty() || short || (size > 0 && body.len() >= size as usize) {
            return Ok(Some((body, internal_date)));
        }
    }
}

/// The progress callback of `ImapExecutor::uid_batch_retrieve_emails`:
/// stored count, average seconds per message, current stall in seconds.
pub type Progress<'a> =
    Option<&'a (dyn Fn(u64, Option<f64>, Option<f64>) -> BichonResult<()> + Send + Sync)>;

/// A warning in the running session's state (the UI shows it). Core unit
/// tests have no store: there it is only logged.
#[cfg(not(test))]
fn session_error(account_id: u64, message: String) {
    let _ = DownloadState::append_session_error(account_id, message);
}

#[cfg(test)]
fn session_error(account_id: u64, message: String) {
    tracing::warn!(account_id, message, "session error (test: not stored)");
}

/// A fetched message: uid, size from the size pass, body, internal date.
type Fetched = (u32, u32, Vec<u8>, i64);

/// One step of the chunked download.
#[derive(Debug, PartialEq, Eq)]
pub enum Step {
    /// Small messages (known size up to a part), fetched whole in one FETCH.
    Group(Vec<(u32, u32)>),
    /// A large or unknown-size message, fetched in parts.
    Single(u32, u32),
}

/// `chunk` is the most one response may carry. Messages up to it are grouped
/// while the group's total stays within it; larger ones and those of unknown
/// size (0) go alone, in parts of `chunk`. How many messages come at once is
/// the account's batch size. Order is kept within each kind.
pub fn plan_steps(messages: &[(u32, u32)], chunk: u32) -> Vec<Step> {
    let mut steps = Vec::new();
    let mut group: Vec<(u32, u32)> = Vec::new();
    let mut group_bytes = 0u64;
    for &(uid, size) in messages {
        if size == 0 || size > chunk {
            steps.push(Step::Single(uid, size));
            continue;
        }
        if !group.is_empty() && group_bytes + u64::from(size) > u64::from(chunk) {
            steps.push(Step::Group(std::mem::take(&mut group)));
            group_bytes = 0;
        }
        group_bytes += u64::from(size);
        group.push((uid, size));
    }
    if !group.is_empty() {
        steps.push(Step::Group(group));
    }
    steps
}

/// Whole bodies of a group of small messages in one FETCH: (uid, body,
/// internal date). Messages gone from the server are simply absent.
pub async fn fetch_group(
    session: &mut Session<Box<dyn SessionStream>>,
    messages: &[(u32, u32)],
) -> BichonResult<Vec<(u32, Vec<u8>, i64)>> {
    let uid_set = messages
        .iter()
        .map(|(uid, _)| uid.to_string())
        .collect::<Vec<_>>()
        .join(",");
    let wanted: std::collections::HashSet<u32> = messages.iter().map(|(uid, _)| *uid).collect();
    let mut stream = session
        .uid_fetch(&uid_set, "(UID INTERNALDATE BODY.PEEK[])")
        .await
        .map_err(|e| raise_error!(format!("{:#?}", e), classify_imap_error(&e)))?;
    let mut bodies = Vec::with_capacity(messages.len());
    while let Some(fetch) = stream
        .try_next()
        .await
        .map_err(|e| raise_error!(format!("{:#?}", e), classify_imap_error(&e)))?
    {
        let Some(uid) = fetch.uid.filter(|uid| wanted.contains(uid)) else {
            continue;
        };
        if let Some(body) = fetch.body() {
            let date = fetch
                .internal_date()
                .map(|d| d.timestamp_millis())
                .unwrap_or(0);
            bodies.push((uid, body.to_vec(), date));
        }
    }
    Ok(bodies)
}

/// Chunked counterpart of the body pass of
/// `ImapExecutor::uid_batch_retrieve_emails`: same outcome, same handling of
/// unparseable messages, same progress reports (one per stored message).
pub async fn retrieve_in_chunks(
    session: &mut Session<Box<dyn SessionStream>>,
    account_id: u64,
    mailbox_id: u64,
    messages: &[(u32, u32)],
    chunk: u32,
    token: &CancellationToken,
    progress: Progress<'_>,
) -> BichonResult<BatchFetchOutcome> {
    let mut outcome = BatchFetchOutcome::default();
    let report = |count: u64| {
        if let Some(progress) = progress {
            let _ = progress(count, None, None);
        }
    };
    for step in plan_steps(messages, chunk) {
        if token.is_cancelled() {
            report(outcome.stored);
            return Err(raise_error!(
                "Stream cancelled".into(),
                ErrorCode::InternalError
            ));
        }
        let fetched: BichonResult<Vec<Fetched>> = match &step {
            Step::Group(group) => fetch_group(session, group).await.map(|bodies| {
                bodies
                    .into_iter()
                    .map(|(uid, body, date)| {
                        let size = group.iter().find(|(u, _)| *u == uid).map_or(0, |(_, s)| *s);
                        (uid, size, body, date)
                    })
                    .collect()
            }),
            Step::Single(uid, size) => fetch_message_in_chunks(session, *uid, *size, chunk)
                .await
                .map(|fetched| {
                    fetched
                        .map(|(body, date)| vec![(*uid, *size, body, date)])
                        .unwrap_or_default()
                }),
        };
        let fetched = match fetched {
            Ok(fetched) => fetched,
            Err(e) => {
                report(outcome.stored);
                return Err(e);
            }
        };
        let expected = match &step {
            Step::Group(group) => group.len(),
            Step::Single(..) => 1,
        };
        // Messages listed by the server whose body did not come: not archived,
        // counted like unparseable ones (the batch reports them, gap-fill
        // retries them) instead of passing silently as gone.
        if fetched.len() < expected {
            let missing = expected - fetched.len();
            outcome.parse_skipped += missing as u64;
            // The server's word on the first missing one of a group (a single
            // message was probed in fetch_message_in_chunks already).
            let server = match &step {
                Step::Group(group) => {
                    let got: std::collections::HashSet<u32> =
                        fetched.iter().map(|(uid, ..)| *uid).collect();
                    match group.iter().find(|(uid, _)| !got.contains(uid)) {
                        Some((uid, _)) => probe(session, *uid).await,
                        None => String::new(),
                    }
                }
                Step::Single(..) => String::new(),
            };
            tracing::warn!(
                account_id,
                mailbox_id,
                missing,
                server,
                "No body from the server for some messages: not archived, gap-fill will retry"
            );
            session_error(
                account_id,
                format!(
                    "{missing} message(s) in mailbox {mailbox_id}: the server returned no body; \
                     not archived, gap-fill will retry"
                ),
            );
        }
        for (uid, size, body, internal_date) in fetched {
            let size = if size > 0 { size } else { body.len() as u32 };
            match extract_raw_and_store_it(&body, uid, size, internal_date, account_id, mailbox_id)
                .await
            {
                Ok(()) => {}
                Err(e) if e.code() == ErrorCode::EmailParseError => {
                    outcome.parse_skipped += 1;
                    tracing::warn!(
                        account_id,
                        mailbox_id,
                        uid,
                        "Skipping unparseable email (not archived)"
                    );
                    let _ = DownloadState::append_session_error(
                        account_id,
                        format!(
                            "Skipped unparseable email (mailbox {mailbox_id}, uid {uid}): \
                             message not archived; gap-fill will keep detecting it"
                        ),
                    );
                    continue;
                }
                Err(e) => {
                    report(outcome.stored);
                    return Err(e);
                }
            }
            outcome.stored += 1;
            if let Some(progress) = progress {
                progress(outcome.stored, None, None)?;
            }
        }
    }
    Ok(outcome)
}

#[cfg(test)]
#[path = "chunked_tests.rs"]
mod tests;
