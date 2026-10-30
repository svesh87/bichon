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

//! svesh: skip work for what is already archived — folders without new mail
//! in the incremental sync, archived UIDs in gap-fill's header scan.
//!
//! Each round already has every folder's STATUS (UIDNEXT, UIDVALIDITY) from
//! the listing connection. A folder whose UIDNEXT has not moved past the last
//! archived UID has no new messages, so the incremental sync need not open a
//! connection (TLS + LOGIN + EXAMINE) for it. With many folders on a slow
//! server (Yandex) that turns a round without new mail from a connection per
//! folder into the listing connection alone.

use crate::archive::imap::mailbox::MailBox;

/// True when the remote folder cannot hold messages newer than the local
/// watermark: same UIDVALIDITY and UIDNEXT at most `highest_uid + 1`.
/// Unknown values (no watermark yet, server without UIDNEXT) never skip.
pub fn has_no_new_mail(local: &MailBox, remote: &MailBox) -> bool {
    match (local.highest_uid, remote.uid_next) {
        (Some(highest), Some(uid_next)) => {
            local.uid_validity.is_some()
                && local.uid_validity == remote.uid_validity
                && u64::from(uid_next) <= u64::from(highest) + 1
        }
        _ => false,
    }
}

/// The remote UIDs gap-fill has to compare by headers. With the same
/// UIDVALIDITY on both sides a UID already archived in this folder is the same
/// message, so only the others need the header scan (one FETCH per batch,
/// slow on Yandex); with a changed or unknown UIDVALIDITY, all of them.
pub fn uids_to_scan(
    local: &MailBox,
    remote: &MailBox,
    remote_uids: &[u32],
    archived_uids: impl IntoIterator<Item = u64>,
) -> Vec<u32> {
    let same_validity = local.uid_validity.is_some() && local.uid_validity == remote.uid_validity;
    if !same_validity {
        return remote_uids.to_vec();
    }
    let archived: std::collections::HashSet<u64> = archived_uids.into_iter().collect();
    remote_uids
        .iter()
        .copied()
        .filter(|uid| !archived.contains(&u64::from(*uid)))
        .collect()
}

#[cfg(test)]
#[path = "unchanged_tests.rs"]
mod tests;
