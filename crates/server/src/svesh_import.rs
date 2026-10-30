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

//! svesh: import messages into an IMAP account's folder under their server
//! UIDs, from a copy kept elsewhere (e.g. a mail client's offline store).
//!
//! Upstream imports only into local (NoSync) accounts. When a server cannot
//! hand out some bodies any more, a client's copy is what is left. Stored like
//! a synced message, with the server's UID, so the sync and gap-fill see it as
//! archived.
//!
//! - `GET  /svesh-api/archived-uids/:account_id/:mailbox_id` — UIDs of the
//!   folder already in the archive.
//! - `POST /svesh-api/import/:account_id/:mailbox_id?uid=&internal_date=`
//!   with the raw message as the body.
//!
//! Outside `/api/v1`, which belongs to the OpenAPI service.
//!
//! Admin or `account:manage` on the account. The caller must only send
//! messages of that folder under the folder's current UIDVALIDITY (it checks
//! that against `/list-mailboxes`).

use std::sync::Arc;

use bichon_core::{
    account::migration::{AccountModel, AccountType},
    archive::imap::mailbox::MailBox,
    common::auth::ClientContext,
    envelope::extractor::extract_raw_and_store_it,
    store::tantivy::envelope::ENVELOPE_MANAGER,
    users::permissions::Permission,
};
use poem::{
    get, handler,
    http::StatusCode,
    post,
    web::{Data, Json, Path, Query},
    Body, EndpointExt, IntoEndpoint, Route,
};
use serde::{Deserialize, Serialize};

use crate::common::auth::ApiGuard;

/// Largest message accepted (same order as the download size limit).
const MAX_MESSAGE_BYTES: usize = 100 * 1024 * 1024;

#[derive(Deserialize)]
struct ImportQuery {
    uid: u32,
    /// Epoch millis; 0 or absent = unknown.
    internal_date: Option<i64>,
}

#[derive(Serialize)]
struct ImportResult {
    uid: u32,
    status: &'static str,
}

fn error(status: StatusCode, message: impl Into<String>) -> poem::Error {
    poem::Error::from_string(message.into(), status)
}

/// The account (IMAP) and its folder, if the caller may manage them.
fn folder(ctx: &ClientContext, account_id: u64, mailbox_id: u64) -> poem::Result<MailBox> {
    if !ctx.has_permission(Some(account_id), Permission::ACCOUNT_MANAGE) {
        return Err(error(StatusCode::FORBIDDEN, "requires account:manage"));
    }
    let account = AccountModel::check_account_exists(account_id)
        .map_err(|e| error(StatusCode::NOT_FOUND, format!("{e:#}")))?;
    if !matches!(account.account_type, AccountType::IMAP) {
        return Err(error(StatusCode::BAD_REQUEST, "only IMAP accounts"));
    }
    MailBox::find_mailbox(account_id, mailbox_id)
        .map_err(|e| error(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?
        .ok_or_else(|| error(StatusCode::NOT_FOUND, "no such folder in this account"))
}

fn archived_uids(account_id: u64, mailbox_id: u64) -> poem::Result<Vec<u64>> {
    let mut uids: Vec<u64> = ENVELOPE_MANAGER
        .get_envelope_snapshots_for_mailbox(account_id, mailbox_id)
        .map_err(|e| error(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?
        .into_iter()
        .map(|s| s.uid)
        .collect();
    uids.sort_unstable();
    uids.dedup();
    Ok(uids)
}

#[handler]
async fn get_archived_uids(
    Path((account_id, mailbox_id)): Path<(u64, u64)>,
    ctx: Data<&Arc<ClientContext>>,
) -> poem::Result<Json<Vec<u64>>> {
    folder(&ctx, account_id, mailbox_id)?;
    Ok(Json(archived_uids(account_id, mailbox_id)?))
}

#[handler]
async fn import_message(
    Path((account_id, mailbox_id)): Path<(u64, u64)>,
    Query(query): Query<ImportQuery>,
    ctx: Data<&Arc<ClientContext>>,
    body: Body,
) -> poem::Result<Json<ImportResult>> {
    folder(&ctx, account_id, mailbox_id)?;
    if query.uid == 0 {
        return Err(error(StatusCode::BAD_REQUEST, "uid must be > 0"));
    }
    if archived_uids(account_id, mailbox_id)?.contains(&u64::from(query.uid)) {
        return Ok(Json(ImportResult {
            uid: query.uid,
            status: "exists",
        }));
    }
    let raw = body
        .into_bytes_limit(MAX_MESSAGE_BYTES)
        .await
        .map_err(|e| error(StatusCode::PAYLOAD_TOO_LARGE, format!("{e}")))?;
    if raw.is_empty() {
        return Err(error(StatusCode::BAD_REQUEST, "empty message"));
    }
    extract_raw_and_store_it(
        &raw,
        query.uid,
        raw.len() as u32,
        query.internal_date.unwrap_or(0),
        account_id,
        mailbox_id,
    )
    .await
    .map_err(|e| error(StatusCode::UNPROCESSABLE_ENTITY, format!("{e:#}")))?;
    tracing::info!(account_id, mailbox_id, uid = query.uid, "Imported message");
    Ok(Json(ImportResult {
        uid: query.uid,
        status: "stored",
    }))
}

/// The routes, behind the API token guard.
pub fn routes() -> impl IntoEndpoint {
    Route::new()
        .at(
            "/svesh-api/archived-uids/:account_id/:mailbox_id",
            get(get_archived_uids),
        )
        .at(
            "/svesh-api/import/:account_id/:mailbox_id",
            post(import_message),
        )
        .with(ApiGuard)
}
