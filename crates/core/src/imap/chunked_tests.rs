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
    // Both messages are gone on the server: nothing stored, no error.
    let token = CancellationToken::new();
    let outcome = retrieve_in_chunks(&mut s, 1, 1, &[(1, 10), (2, 0)], 4, &token, Some(&progress))
        .await
        .unwrap();
    assert_eq!(outcome, BatchFetchOutcome::default());

    token.cancel();
    let err = retrieve_in_chunks(&mut s, 1, 1, &[(1, 10)], 4, &token, Some(&progress)).await;
    assert!(err.is_err());
    assert_eq!(*seen.lock().unwrap(), vec![0]);
}
