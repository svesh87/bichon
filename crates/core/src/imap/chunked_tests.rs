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

//! Tests of `chunked` against the scripted IMAP server of `mock_server`.

use tokio_io_timeout::TimeoutStream;

use super::*;
use crate::imap::mock_server::{MockImapServer, MockImapServerHandle};

#[test]
fn chunk_setting_parses_kilobytes() {
    assert_eq!(parse_chunk_kb(None), None);
    assert_eq!(parse_chunk_kb(Some("")), None);
    assert_eq!(parse_chunk_kb(Some("0")), None);
    assert_eq!(parse_chunk_kb(Some("abc")), None);
    assert_eq!(parse_chunk_kb(Some(" 128 ")), Some(128 * 1024));
    assert_eq!(parse_chunk_kb(Some("1")), Some(4 * 1024));
    assert_eq!(parse_chunk_kb(Some("999999")), Some(65_536 * 1024));
}

#[test]
fn account_setting_wins_over_server_default() {
    let server = Some(128 * 1024);
    assert_eq!(effective_chunk(None, server), server);
    assert_eq!(effective_chunk(None, None), None);
    assert_eq!(effective_chunk(Some(0), server), None, "0 turns it off");
    assert_eq!(effective_chunk(Some(64), None), Some(64 * 1024));
    assert_eq!(effective_chunk(Some(64), server), Some(64 * 1024));
    // chunk_for_account reads the store: tested in the server's MCP tests.
}

async fn session(handle: &MockImapServerHandle) -> Session<Box<dyn SessionStream>> {
    let tcp = tokio::net::TcpStream::connect((handle.host(), handle.port()))
        .await
        .unwrap();
    let pinned: std::pin::Pin<Box<TimeoutStream<tokio::net::TcpStream>>> =
        Box::pin(TimeoutStream::new(tcp));
    let stream: Box<dyn SessionStream> = Box::new(pinned);
    let mut client = async_imap::Client::new(stream);
    client.read_response().await.unwrap();
    let mut session = client
        .login("user", "pass")
        .await
        .map_err(|(e, _)| panic!("login: {e:?}"))
        .unwrap();
    session.examine("INBOX").await.unwrap();
    session
}

fn part(uid: u32, offset: usize, data: &str, with_date: bool) -> Vec<u8> {
    let date = if with_date {
        "INTERNALDATE \"06-Oct-2026 10:00:00 +0000\" "
    } else {
        ""
    };
    format!(
        "* 1 FETCH (UID {uid} {date}BODY[]<{offset}> {{{}}}\r\n{data})\r\n{{TAG}} OK FETCH done\r\n",
        data.len()
    )
    .into_bytes()
}

fn mock() -> MockImapServer {
    MockImapServer::new()
        .respond("LOGIN", "{TAG} OK LOGIN done\r\n")
        .respond(
            "EXAMINE",
            "* 1 EXISTS\r\n* OK [UIDVALIDITY 1] ok\r\n{TAG} OK [READ-ONLY] done\r\n",
        )
}

#[tokio::test]
async fn assembles_a_message_from_parts() {
    // 10 bytes in parts of 4: "Subj", "ect:", then the short last part " x".
    let handle = mock()
        .respond("<0.4>", part(7, 0, "Subj", true))
        .respond("<4.4>", part(7, 4, "ect:", false))
        .respond("<8.4>", part(7, 8, " x", false))
        .start()
        .await;
    let mut s = session(&handle).await;
    let (body, date) = fetch_message_in_chunks(&mut s, 7, 0, 4)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(body, b"Subject: x");
    assert_eq!(date, 1_791_280_800_000);

    // Known size: stops once it is reached, without asking past the end.
    let (body, _) = fetch_message_in_chunks(&mut s, 7, 8, 4)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(body, b"Subject:");
}

#[tokio::test]
async fn gone_message_other_uids_and_missing_parts() {
    let handle = mock()
        // A FETCH for another message only (unsolicited): ours is gone.
        .respond("UID FETCH 9 ", part(3, 0, "noise", true))
        // The first part is full, the second never comes.
        .respond("<0.4>", part(5, 0, "abcd", true))
        .start()
        .await;
    let mut s = session(&handle).await;
    assert!(fetch_message_in_chunks(&mut s, 9, 0, 4)
        .await
        .unwrap()
        .is_none());
    let err = fetch_message_in_chunks(&mut s, 5, 0, 4).await.unwrap_err();
    assert!(format!("{err:?}").contains("no data at offset 4"));
}

#[tokio::test]
async fn refused_fetch_counts_as_no_body() {
    // async-imap ends the stream on a tagged NO without an error: the message
    // is skipped like a vanished one, and gap-fill finds it later.
    let handle = mock()
        .respond("UID FETCH", "{TAG} NO [UNAVAILABLE] try later\r\n")
        .start()
        .await;
    let mut s = session(&handle).await;
    assert!(fetch_message_in_chunks(&mut s, 1, 0, 4)
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn no_body_in_parts_falls_back_to_whole() {
    // Yandex answers a partial BODY.PEEK[]<0.N> of some messages without a
    // body; the whole message then comes with a plain BODY.PEEK[].
    let whole =
        "* 1 FETCH (UID 6 INTERNALDATE \"06-Oct-2026 10:00:00 +0000\" BODY[] {5}\r\nhello)\r\n\
                 {TAG} OK FETCH done\r\n";
    let handle = mock()
        .respond(
            "<0.4>",
            "* 1 FETCH (UID 6 FLAGS (\\Seen))\r\n{TAG} OK FETCH done\r\n",
        )
        .respond("BODY.PEEK[])", whole)
        .start()
        .await;
    let mut s = session(&handle).await;
    let (body, date) = fetch_message_in_chunks(&mut s, 6, 5, 4)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(body, b"hello");
    assert_eq!(date, 1_791_280_800_000);
}

#[tokio::test]
async fn retrieve_reports_errors_and_progress() {
    // The second part of message 1 never comes: an error, after reporting
    // what was stored so far (nothing).
    let handle = mock()
        .respond("<0.4>", part(1, 0, "abcd", true))
        .start()
        .await;
    let mut s = session(&handle).await;
    let seen = std::sync::Mutex::new(Vec::new());
    let progress = |count: u64, _: Option<f64>, _: Option<f64>| -> BichonResult<()> {
        seen.lock().unwrap().push(count);
        Ok(())
    };
    let token = CancellationToken::new();
    let result = retrieve_in_chunks(&mut s, 1, 1, &[(1, 10)], 4, &token, Some(&progress)).await;
    assert!(result.is_err());
    assert_eq!(*seen.lock().unwrap(), vec![0]);
}

#[tokio::test]
async fn retrieve_skips_gone_messages_and_stops_when_cancelled() {
    let handle = mock()
        .respond("UID FETCH", "{TAG} OK FETCH done\r\n")
        .start()
        .await;
    let mut s = session(&handle).await;
    let seen = std::sync::Mutex::new(Vec::new());
    let progress = |count: u64, _: Option<f64>, _: Option<f64>| -> BichonResult<()> {
        seen.lock().unwrap().push(count);
        Ok(())
    };
    // No body for either message (not in parts, not whole): nothing stored,
    // no error, both counted as not archived so gap-fill retries them.
    let token = CancellationToken::new();
    let outcome = retrieve_in_chunks(&mut s, 1, 1, &[(1, 10), (2, 0)], 4, &token, Some(&progress))
        .await
        .unwrap();
    assert_eq!(outcome.stored, 0);
    assert_eq!(outcome.parse_skipped, 2);

    token.cancel();
    let err = retrieve_in_chunks(&mut s, 1, 1, &[(1, 10)], 4, &token, Some(&progress)).await;
    assert!(err.is_err());
    assert_eq!(*seen.lock().unwrap(), vec![0]);
}

#[test]
fn small_messages_are_grouped_large_ones_go_alone() {
    let chunk = 100;
    let steps = plan_steps(&[(1, 10), (2, 200), (3, 0), (4, 100), (5, 50)], chunk);
    assert_eq!(
        steps,
        vec![
            Step::Single(2, 200),
            Step::Single(3, 0),
            Step::Group(vec![(1, 10), (4, 100), (5, 50)]),
        ]
    );

    // A group closes at GROUP_BYTES ...
    let half = (GROUP_BYTES / 2) as u32;
    let steps = plan_steps(&[(1, half), (2, half), (3, 1)], half);
    assert_eq!(
        steps,
        vec![
            Step::Group(vec![(1, half), (2, half)]),
            Step::Group(vec![(3, 1)])
        ]
    );
    // ... and at GROUP_MAX messages.
    let many: Vec<(u32, u32)> = (1..=GROUP_MAX as u32 + 1).map(|uid| (uid, 1)).collect();
    let steps = plan_steps(&many, chunk);
    assert_eq!(steps.len(), 2);
    assert!(matches!(&steps[1], Step::Group(g) if g == &vec![(GROUP_MAX as u32 + 1, 1)]));
    assert!(plan_steps(&[], chunk).is_empty());
}

#[tokio::test]
async fn group_fetch_returns_every_body() {
    let response =
        "* 1 FETCH (UID 4 INTERNALDATE \"06-Oct-2026 10:00:00 +0000\" BODY[] {3}\r\nabc)\r\n\
         * 2 FETCH (UID 9 BODY[] {2}\r\nxy)\r\n\
         * 3 FETCH (UID 7 FLAGS (\\Seen))\r\n\
         {TAG} OK FETCH done\r\n";
    let handle = mock().respond("UID FETCH 4,9,5", response).start().await;
    let mut s = session(&handle).await;
    let bodies = fetch_group(&mut s, &[(4, 3), (9, 2), (5, 1)])
        .await
        .unwrap();
    // UID 5 is gone, UID 7 is an unsolicited flag update.
    assert_eq!(
        bodies,
        vec![
            (4, b"abc".to_vec(), 1_791_280_800_000),
            (9, b"xy".to_vec(), 0)
        ]
    );
}

#[test]
fn gentle_mode_only_with_a_chunk_size() {
    use crate::account::migration::AccountModel;
    let mut account = AccountModel::default();
    // No setting (and no server default in tests): upstream behaviour.
    assert_eq!(is_gentle(&account), CHUNK_BYTES.is_some());
    account.imap_fetch_chunk_kb = Some(0);
    assert!(!is_gentle(&account), "0 = off");
    account.imap_fetch_chunk_kb = Some(128);
    assert!(is_gentle(&account));
}

#[tokio::test]
async fn retrieve_groups_small_messages() {
    // Small messages go in one shared FETCH; here no body comes: nothing
    // stored, no error, all three counted as not archived.
    let handle = mock()
        .respond("UID FETCH 1,2,3", "{TAG} OK FETCH done\r\n")
        .start()
        .await;
    let mut s = session(&handle).await;
    let token = CancellationToken::new();
    let outcome = retrieve_in_chunks(&mut s, 1, 1, &[(1, 3), (2, 3), (3, 3)], 4, &token, None)
        .await
        .unwrap();
    assert_eq!(outcome.stored, 0);
    assert_eq!(outcome.parse_skipped, 3);

    // A broken group response is an error, not a silent skip.
    let handle = mock()
        .respond("UID FETCH", "* 1 FETCH (UID 1 BODY[] {100}\r\nshort")
        .start()
        .await;
    let mut s = session(&handle).await;
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        retrieve_in_chunks(&mut s, 1, 1, &[(1, 3), (2, 3)], 4, &token, None),
    )
    .await;
    assert!(!matches!(result, Ok(Ok(_))));
}
