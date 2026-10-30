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

use super::*;

fn mailbox(uid_validity: Option<u32>, uid_next: Option<u32>, highest: Option<u32>) -> MailBox {
    MailBox {
        uid_validity,
        uid_next,
        highest_uid: highest,
        ..Default::default()
    }
}

#[test]
fn skips_only_when_nothing_can_be_new() {
    let local = mailbox(Some(7), None, Some(100));
    assert!(has_no_new_mail(&local, &mailbox(Some(7), Some(101), None)));
    assert!(has_no_new_mail(&local, &mailbox(Some(7), Some(50), None)));
    assert!(!has_no_new_mail(&local, &mailbox(Some(7), Some(102), None)));
    // UIDVALIDITY changed or unknown: the full reconcile path decides.
    assert!(!has_no_new_mail(&local, &mailbox(Some(8), Some(101), None)));
    assert!(!has_no_new_mail(&local, &mailbox(None, Some(101), None)));
    assert!(!has_no_new_mail(
        &mailbox(None, None, Some(100)),
        &mailbox(None, Some(101), None)
    ));
    // No watermark yet, or no UIDNEXT from the server.
    assert!(!has_no_new_mail(
        &mailbox(Some(7), None, None),
        &mailbox(Some(7), Some(1), None)
    ));
    assert!(!has_no_new_mail(&local, &mailbox(Some(7), None, None)));
    // Highest UID at the top of the range does not overflow.
    let top = mailbox(Some(7), None, Some(u32::MAX));
    assert!(has_no_new_mail(
        &top,
        &mailbox(Some(7), Some(u32::MAX), None)
    ));
}
