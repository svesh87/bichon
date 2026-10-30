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

//! svesh: skip folders without new mail.
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

#[cfg(test)]
#[path = "unchanged_tests.rs"]
mod tests;
