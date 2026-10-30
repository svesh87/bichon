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

//! svesh: chunked body download, opt-in (`BICHON_IMAP_FETCH_CHUNK_KB`).
//!
//! The default download asks for the whole bodies of a batch of messages in
//! one `UID FETCH`. Some servers (Yandex) stall on such large responses and
//! drop the connection mid-transfer. In chunked mode each message is fetched
//! on its own, in parts of `BODY.PEEK[]<offset.length>`, as Thunderbird does
//! (`mail.imap.chunk_size`, 136 KB by default): small responses, steady
//! traffic, and a dropped connection costs one part instead of a batch.

use std::sync::LazyLock;

use async_imap::Session;
use futures::TryStreamExt;
use tokio_util::sync::CancellationToken;

use crate::{
    account::state::DownloadState,
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

/// The whole body of one message, fetched in parts. `None` when the server
/// returns no body (the message is gone). The internal date (epoch millis)
/// comes with the first part. A part missing after the first one is an error:
/// a truncated message is never returned.
pub async fn fetch_message_in_chunks(
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
    for &(uid, size) in messages {
        if token.is_cancelled() {
            report(outcome.stored);
            return Err(raise_error!(
                "Stream cancelled".into(),
                ErrorCode::InternalError
            ));
        }
        let fetched = match fetch_message_in_chunks(session, uid, size, chunk).await {
            Ok(fetched) => fetched,
            Err(e) => {
                report(outcome.stored);
                return Err(e);
            }
        };
        let Some((body, internal_date)) = fetched else {
            tracing::warn!(
                account_id,
                mailbox_id,
                uid,
                "FETCH returned no body, skipping message"
            );
            continue;
        };
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
    Ok(outcome)
}

#[cfg(test)]
#[path = "chunked_tests.rs"]
mod tests;
