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

//! Tests of the MCP module: pure helpers, the tools against a real (temporary)
//! store with imported messages, the HTTP endpoint, and drafts against a tiny
//! scripted IMAP server.

use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

use base64::Engine;
use bichon_core::{
    archive::imap::mailbox::MailBox,
    common::auth::ClientContext,
    import::{BatchEmlRequest, ImportEmls},
    store::tantivy::{attachment::ATTACHMENT_MANAGER, envelope::ENVELOPE_MANAGER},
    users::{UserModel, DEFAULT_ADMIN_USER_ID},
};
use mail_parser::{MessageParser, MimeHeaders};
use poem::{
    endpoint::make_sync, http::StatusCode, test::TestClient, web::Json as PoemJson, EndpointExt,
    IntoResponse, Response, Route,
};
use serde_json::{json, Value};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::TcpListener,
    sync::OnceCell,
};

use super::{
    draft::{
        append_draft, build_draft, read_attachments, reply_context, reply_subject, save_draft,
        DraftRequest, ReplyContext,
    },
    files::{resolve_input, safe_file_name, save},
    mcp_endpoint,
    tools::{format_ms, parse_date, BichonMcpTools},
    unbatch, MCP_SETTINGS,
};
use crate::common::auth::ApiGuard;

// ── Fixtures ────────────────────────────────────────────────────────────────

const ORIGINAL: &str = "Message-ID: <orig-1@example.org>\r\n\
From: Alice <alice@example.org>\r\n\
Reply-To: Alice Desk <desk@example.org>\r\n\
To: archive@example.com\r\n\
Subject: =?UTF-8?B?0JDQutGCINC30LAg0L7QutGC0Y/QsdGA0Yw=?=\r\n\
Date: Tue, 06 Oct 2026 10:00:00 +0000\r\n\
MIME-Version: 1.0\r\n\
Content-Type: multipart/mixed; boundary=\"b1\"\r\n\
\r\n\
--b1\r\n\
Content-Type: text/plain; charset=utf-8\r\n\
\r\n\
Akt attached, zebra-marker\r\n\
--b1\r\n\
Content-Type: application/pdf\r\n\
Content-Disposition: attachment; filename=\"akt.pdf\"\r\n\
Content-Transfer-Encoding: base64\r\n\
\r\n\
UERGREFUQQ==\r\n\
--b1--\r\n";

const REPLY: &str = "Message-ID: <orig-2@example.org>\r\n\
In-Reply-To: <orig-1@example.org>\r\n\
References: <orig-1@example.org>\r\n\
From: archive@example.com\r\n\
To: Alice <alice@example.org>\r\n\
Cc: boss@example.org\r\n\
Subject: Re: =?UTF-8?B?0JDQutGCINC30LAg0L7QutGC0Y/QsdGA0Yw=?=\r\n\
Date: Wed, 07 Oct 2026 10:00:00 +0000\r\n\
MIME-Version: 1.0\r\n\
Content-Type: text/html; charset=utf-8\r\n\
\r\n\
<p>Thanks, got it</p>\r\n";

struct Fixture {
    archive_id: u64,
    original_id: String,
    reply_id: String,
    thread_id: String,
}

static FIXTURE: OnceCell<Fixture> = OnceCell::const_new();

fn files_root() -> PathBuf {
    std::env::temp_dir().join(format!("bichon-mcp-files-{}", std::process::id()))
}

/// The MCP file directory, set before `MCP_SETTINGS` is first read.
fn files_env() {
    static ENV: std::sync::Once = std::sync::Once::new();
    ENV.call_once(|| {
        std::fs::create_dir_all(files_root()).unwrap();
        std::env::set_var("BICHON_MCP_FILES_DIR", files_root());
        std::env::set_var("BICHON_ENABLE_MCP", "true");
    });
}

/// The shared server setup. Only `store_scenarios` calls it: the index
/// writer runs as a task of the runtime that first starts the store, so every
/// test that needs the index runs inside that one test.
async fn setup() {
    files_env();
    crate::tests::setup().await;
}

fn admin() -> ClientContext {
    ClientContext {
        ip_addr: None,
        user: UserModel::find(DEFAULT_ADMIN_USER_ID).unwrap().unwrap(),
    }
}

/// A logged-in user without any role.
fn nobody() -> ClientContext {
    let mut user = UserModel::find(DEFAULT_ADMIN_USER_ID).unwrap().unwrap();
    user.global_roles = Default::default();
    user.account_access_map = Default::default();
    ClientContext {
        ip_addr: None,
        user,
    }
}

fn api() -> TestClient<impl poem::Endpoint> {
    TestClient::new(crate::tests::build_api_route()).default_header("X-Forwarded-For", "127.0.0.1")
}

async fn create_account(body: Value) -> u64 {
    let token = crate::tests::admin_token().await;
    let resp = api()
        .post("/api/v1/account")
        .header("Authorization", format!("Bearer {token}"))
        .body_json(&body)
        .send()
        .await;
    if !resp.0.status().is_success() {
        let status = resp.0.status();
        panic!(
            "create account: {status} {}",
            resp.0.into_body().into_string().await.unwrap()
        );
    }
    resp.json().await.value().object().get("id").i64() as u64
}

fn b64(eml: &str) -> String {
    // The import API takes URL-safe base64.
    base64::engine::general_purpose::URL_SAFE.encode(eml)
}

async fn search_ids(ctx: &ClientContext, text: Option<&str>, account: Option<u64>) -> Vec<Value> {
    let page = BichonMcpTools::new(ctx.clone())
        .search_emails(
            text.map(Into::into),
            None,
            None,
            None,
            None,
            None,
            None,
            account.map(|id| vec![id]),
            None,
            None,
            None,
            None,
            None,
        )
        .await
        .unwrap();
    serde_json::to_value(page.0).unwrap()["messages"]
        .as_array()
        .unwrap()
        .clone()
}

/// A local archive account with the two messages above, searchable.
async fn fixture() -> &'static Fixture {
    FIXTURE
        .get_or_init(|| async {
            setup().await;
            let archive_id = create_account(json!({
                "email": "archive@example.com",
                "enabled": true,
                "account_type": "NoSync",
                "use_dangerous": false,
                "account_name": "Archive"
            }))
            .await;
            let result = ImportEmls::do_import(BatchEmlRequest {
                account_id: archive_id,
                mail_folder: "INBOX".into(),
                emls: vec![b64(ORIGINAL), b64(REPLY)],
            })
            .await
            .unwrap();
            assert_eq!(result.success, 2, "{result:?}");
            // The index commits once a minute: force it, then wait for the reader.
            ENVELOPE_MANAGER.prepare_for_backup().await.unwrap();
            ATTACHMENT_MANAGER.prepare_for_backup().await.unwrap();
            let ctx = admin();
            let mut found = Vec::new();
            for _ in 0..100 {
                found = search_ids(&ctx, None, Some(archive_id)).await;
                if found.len() >= 2 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            let by_subject = |prefix: &str| {
                found
                    .iter()
                    .find(|m| m["subject"].as_str().unwrap().starts_with(prefix))
                    .unwrap_or_else(|| panic!("no message {prefix:?} in {found:?}"))
                    .clone()
            };
            let original = by_subject("Акт");
            let reply = by_subject("Re:");
            Fixture {
                archive_id,
                original_id: original["envelope_id"].as_str().unwrap().into(),
                reply_id: reply["envelope_id"].as_str().unwrap().into(),
                thread_id: original["thread_id"].as_str().unwrap().into(),
            }
        })
        .await
}

// ── Scripted IMAP server ────────────────────────────────────────────────────

type Appended = Arc<Mutex<Vec<(String, String, Vec<u8>)>>>;

/// Answers what Bichon sends for login, LIST, EXAMINE, SEARCH and APPEND;
/// records appended messages. `drafts`: whether a folder carries `\Drafts`.
async fn mock_imap(drafts: bool) -> (u16, Appended) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let appended: Appended = Default::default();
    let store = appended.clone();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let store = store.clone();
            tokio::spawn(async move {
                let _ = imap_session(stream, drafts, store).await;
            });
        }
    });
    (port, appended)
}

async fn imap_session(
    stream: tokio::net::TcpStream,
    drafts: bool,
    store: Appended,
) -> std::io::Result<()> {
    let (read, mut write) = stream.into_split();
    let mut read = BufReader::new(read);
    write
        .write_all(b"* OK [CAPABILITY IMAP4rev1 AUTH=PLAIN] mock ready\r\n")
        .await?;
    let mut line = String::new();
    loop {
        line.clear();
        if read.read_line(&mut line).await? == 0 {
            return Ok(());
        }
        let mut words = line.trim_end().splitn(3, ' ');
        let tag = words.next().unwrap_or("*").to_string();
        let mut command = words.next().unwrap_or("").to_ascii_uppercase();
        let mut rest = words.next().unwrap_or("").to_string();
        if command == "UID" {
            let mut sub = rest.splitn(2, ' ');
            command = sub.next().unwrap_or("").to_ascii_uppercase();
            rest = sub.next().unwrap_or("").to_string();
        }
        let reply = match command.as_str() {
            "CAPABILITY" => "* CAPABILITY IMAP4rev1 AUTH=PLAIN ID\r\n".to_string(),
            "AUTHENTICATE" => {
                write.write_all(b"+ \r\n").await?;
                let mut ignored = String::new();
                read.read_line(&mut ignored).await?;
                String::new()
            }
            "LIST" => {
                let drafts_attr = if drafts { " \\Drafts" } else { "" };
                format!(
                    "* LIST (\\HasNoChildren) \"/\" \"INBOX\"\r\n\
                     * LIST (\\HasNoChildren{drafts_attr}) \"/\" \"Drafts\"\r\n"
                )
            }
            "SELECT" | "EXAMINE" => "* 0 EXISTS\r\n* 0 RECENT\r\n* FLAGS (\\Seen \\Draft)\r\n\
                 * OK [UIDVALIDITY 1] ok\r\n* OK [UIDNEXT 1] ok\r\n"
                .to_string(),
            "SEARCH" => "* SEARCH\r\n".to_string(),
            "STATUS" => {
                "* STATUS \"INBOX\" (MESSAGES 0 UIDNEXT 1 UIDVALIDITY 1 UNSEEN 0)\r\n".to_string()
            }
            "APPEND" => {
                // APPEND <mailbox> (<flags>) {<n>} or {<n>+}
                let mailbox = rest
                    .split(' ')
                    .next()
                    .unwrap_or("")
                    .trim_matches('"')
                    .to_string();
                let flags = rest
                    .find('(')
                    .and_then(|start| {
                        rest[start..]
                            .find(')')
                            .map(|end| &rest[start..=start + end])
                    })
                    .unwrap_or("")
                    .to_string();
                let size_spec = &rest[rest.rfind('{').unwrap() + 1..rest.rfind('}').unwrap()];
                let synchronizing = !size_spec.ends_with('+');
                let size: usize = size_spec.trim_end_matches('+').parse().unwrap();
                if synchronizing {
                    write.write_all(b"+ go ahead\r\n").await?;
                }
                let mut body = vec![0u8; size];
                read.read_exact(&mut body).await?;
                let mut end = String::new();
                read.read_line(&mut end).await?;
                store.lock().unwrap().push((mailbox, flags, body));
                String::new()
            }
            "LOGOUT" => {
                write
                    .write_all(format!("* BYE\r\n{tag} OK bye\r\n").as_bytes())
                    .await?;
                return Ok(());
            }
            _ => String::new(),
        };
        write
            .write_all(format!("{reply}{tag} OK done\r\n").as_bytes())
            .await?;
    }
}

async fn imap_account(email: &str, drafts: bool) -> (u64, Appended) {
    setup().await;
    let (port, appended) = mock_imap(drafts).await;
    let id = create_account(json!({
        "email": email,
        "enabled": true,
        "account_type": "IMAP",
        "use_dangerous": false,
        "account_name": "Draft Tester",
        "download_interval_min": 600,
        "imap": {
            "host": "127.0.0.1",
            "port": port,
            "encryption": "None",
            "auth": {"auth_type": "Password", "password": "secret"}
        }
    }))
    .await;
    (id, appended)
}

// ── Pure helpers ────────────────────────────────────────────────────────────

#[test]
fn reply_subject_adds_prefix_once() {
    assert_eq!(reply_subject("Акт за октябрь"), "Re: Акт за октябрь");
    assert_eq!(reply_subject("RE: Акт"), "RE: Акт");
    assert_eq!(reply_subject("Re[2]: Акт"), "Re[2]: Акт");
}

#[test]
fn parse_date_accepts_day_and_rfc3339() {
    assert_eq!(parse_date("1970-01-02").unwrap(), 86_400_000);
    assert_eq!(parse_date(" 1970-01-01T00:00:01Z ").unwrap(), 1000);
    assert!(parse_date("01.02.2026").is_err());
}

#[test]
fn format_ms_is_rfc3339_or_empty() {
    assert_eq!(format_ms(0), "1970-01-01T00:00:00+00:00");
    assert_eq!(format_ms(i64::MAX), "");
}

#[test]
fn safe_file_name_strips_paths_and_controls() {
    assert_eq!(safe_file_name("../../etc/passwd"), "_.._etc_passwd");
    assert_eq!(safe_file_name("..hidden"), "hidden");
    assert_eq!(safe_file_name("a:b*c?\u{7}.pdf"), "a_b_c__.pdf");
    assert_eq!(safe_file_name("   "), "attachment");
    assert_eq!(safe_file_name(&"я".repeat(200)).chars().count(), 150);
}

#[test]
fn reply_context_reads_threading_headers() {
    let ctx = reply_context(REPLY.as_bytes()).unwrap();
    assert_eq!(ctx.message_id.as_deref(), Some("orig-2@example.org"));
    assert_eq!(ctx.references, ["orig-1@example.org", "orig-2@example.org"]);
    assert_eq!(ctx.reply_to, ["archive@example.com"]);
    let long =
        reply_context(b"Message-ID: <c@x>\r\nReferences: <a@x> <b@x>\r\nFrom: f@x\r\n\r\nbody")
            .unwrap();
    assert_eq!(long.references, ["a@x", "b@x", "c@x"]);
    assert!(ctx.subject.starts_with("Re: Акт"));

    let original = reply_context(ORIGINAL.as_bytes()).unwrap();
    assert_eq!(
        original.reply_to,
        ["desk@example.org"],
        "Reply-To wins over From"
    );
    assert_eq!(original.references, ["orig-1@example.org"]);

    let bare = reply_context(b"Subject: x\r\n\r\nbody").unwrap();
    assert!(bare.message_id.is_none() && bare.references.is_empty() && bare.reply_to.is_empty());
    assert!(reply_context(b"").is_err());
}

fn parse(raw: &[u8]) -> mail_parser::Message<'_> {
    MessageParser::default().parse(raw).unwrap()
}

#[test]
fn build_draft_reply_sets_threading_and_defaults() {
    let reply = ReplyContext {
        message_id: Some("orig-1@example.org".into()),
        references: vec!["orig-0@example.org".into(), "orig-1@example.org".into()],
        subject: "Акт".into(),
        reply_to: vec!["alice@example.org".into()],
    };
    let req = DraftRequest {
        cc: vec!["cc@example.org".into()],
        bcc: vec!["bcc@example.org".into()],
        text: Some("Спасибо".into()),
        html: Some("<p>Спасибо</p>".into()),
        ..Default::default()
    };
    let files = vec![("application/pdf".into(), "akt.pdf".into(), b"PDF".to_vec())];
    let built = build_draft("me@example.com", Some("Me"), &req, Some(&reply), files).unwrap();
    assert_eq!(built.to, ["alice@example.org"]);
    assert_eq!(built.subject, "Re: Акт");
    assert_eq!(built.attachments, ["akt.pdf"]);
    assert!(built.message_id.ends_with("@example.com"));

    let msg = parse(&built.raw);
    assert_eq!(msg.subject(), Some("Re: Акт"));
    assert_eq!(msg.in_reply_to().as_text(), Some("orig-1@example.org"));
    assert_eq!(msg.message_id(), Some(built.message_id.as_str()));
    let from = msg.from().unwrap().first().unwrap();
    assert_eq!(from.name(), Some("Me"));
    assert_eq!(from.address(), Some("me@example.com"));
    assert_eq!(
        msg.cc().unwrap().first().unwrap().address(),
        Some("cc@example.org")
    );
    assert_eq!(msg.body_text(0).as_deref(), Some("Спасибо"));
    assert_eq!(
        msg.attachment(0).unwrap().attachment_name(),
        Some("akt.pdf")
    );
}

#[test]
fn build_draft_new_message_and_errors() {
    let req = DraftRequest {
        to: vec!["x@example.org".into()],
        subject: Some("Hello".into()),
        text: Some("Hi".into()),
        ..Default::default()
    };
    let built = build_draft("me@example.com", Some("  "), &req, None, vec![]).unwrap();
    let msg = parse(&built.raw);
    assert_eq!(
        msg.from().unwrap().first().unwrap().name(),
        None,
        "blank name is dropped"
    );
    assert!(msg.in_reply_to().is_empty());
    let built = build_draft("me@example.com", None, &req, None, vec![]).unwrap();
    assert_eq!(built.subject, "Hello");

    let no_to = DraftRequest {
        to: vec![],
        ..DraftRequest {
            subject: Some("s".into()),
            text: Some("t".into()),
            ..Default::default()
        }
    };
    assert!(build_draft("me@example.com", None, &no_to, None, vec![])
        .unwrap_err()
        .contains("No recipients"));
    let no_subject = DraftRequest {
        to: vec!["x@example.org".into()],
        text: Some("t".into()),
        ..Default::default()
    };
    assert!(
        build_draft("me@example.com", None, &no_subject, None, vec![])
            .unwrap_err()
            .contains("No subject")
    );
    let no_body = DraftRequest {
        to: vec!["x@example.org".into()],
        subject: Some("s".into()),
        ..Default::default()
    };
    assert!(build_draft("me@example.com", None, &no_body, None, vec![])
        .unwrap_err()
        .contains("No body"));
}

// ── File exchange directory ─────────────────────────────────────────────────

#[test]
fn files_save_and_resolve_stay_inside_the_directory() {
    files_env();
    assert!(MCP_SETTINGS.enabled);
    let path = save(&["t", "a/b"], "../report.pdf", b"data").unwrap();
    let root = files_root().canonicalize().unwrap();
    assert!(path.starts_with(&root));
    assert_eq!(path.file_name().unwrap(), "_report.pdf");
    assert_eq!(std::fs::read(&path).unwrap(), b"data");
    let leftovers: Vec<_> = std::fs::read_dir(path.parent().unwrap())
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().ends_with(".part"))
        .collect();
    assert!(leftovers.is_empty());

    // Relative and absolute paths inside are fine.
    assert_eq!(resolve_input("t/a_b/_report.pdf").unwrap(), path);
    assert_eq!(resolve_input(path.to_str().unwrap()).unwrap(), path);
    // Outside, directories, missing files and symlinks out are refused.
    assert!(resolve_input("/etc/hostname")
        .unwrap_err()
        .contains("outside"));
    assert!(resolve_input("t")
        .unwrap_err()
        .contains("not a regular file"));
    assert!(resolve_input("t/missing.pdf").is_err());
    let link = root.join("t/escape");
    let _ = std::fs::remove_file(&link);
    std::os::unix::fs::symlink("/etc/hostname", &link).unwrap();
    assert!(resolve_input("t/escape").unwrap_err().contains("outside"));
    // A target that cannot be written: a directory in the way of the file.
    std::fs::create_dir_all(root.join("t/blocked/name")).unwrap();
    assert!(save(&["t", "blocked"], "name", b"x")
        .unwrap_err()
        .contains("Rename"));
}

#[test]
fn read_attachments_guesses_types_and_caps_size() {
    files_env();
    let pdf = save(&["att"], "doc.pdf", b"%PDF").unwrap();
    let odd = save(&["att"], "blob.zzzunknown", b"?").unwrap();
    let files = read_attachments(&[pdf.display().to_string(), odd.display().to_string()]).unwrap();
    assert_eq!(files[0].0, "application/pdf");
    assert_eq!(files[0].1, "doc.pdf");
    assert_eq!(files[1].0, "application/octet-stream");

    let big = save(&["att"], "big.bin", &vec![0u8; 19 * 1024 * 1024]).unwrap();
    assert!(read_attachments(&[big.display().to_string()])
        .unwrap_err()
        .contains("exceed"));
    assert!(read_attachments(&["/etc/hostname".into()]).is_err());
}

// ── unbatch ─────────────────────────────────────────────────────────────────

async fn through_unbatch(resp: Response) -> (StatusCode, String) {
    let resp = Arc::new(Mutex::new(Some(resp)));
    let ep = make_sync(move |_| resp.lock().unwrap().take().unwrap()).around(unbatch);
    let out = TestClient::new(ep).post("/").send().await;
    let status = out.0.status();
    (status, out.0.into_body().into_string().await.unwrap())
}

#[tokio::test]
async fn unbatch_normalizes_single_responses() {
    let json = |v: Value| PoemJson(v).into_response();
    assert_eq!(
        through_unbatch(json(json!([]))).await,
        (StatusCode::ACCEPTED, String::new())
    );
    assert_eq!(
        through_unbatch(json(json!([{"id": 1}]))).await,
        (StatusCode::OK, r#"{"id":1}"#.into())
    );
    assert_eq!(through_unbatch(json(json!([1, 2]))).await.1, "[1,2]");
    assert_eq!(through_unbatch("plain".into_response()).await.1, "plain");
    let failed = PoemJson(json!([]))
        .with_status(StatusCode::BAD_REQUEST)
        .into_response();
    assert_eq!(through_unbatch(failed).await.0, StatusCode::BAD_REQUEST);
    let broken = "[oops"
        .with_content_type("application/json")
        .into_response();
    assert_eq!(through_unbatch(broken).await.1, "[oops");
}

// ── Tools against the store ─────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn store_scenarios() {
    fixture().await;
    list_accounts_and_folders().await;
    search_filters_and_errors().await;
    get_email_and_thread().await;
    save_attachments_writes_files().await;
    save_draft_appends_to_drafts_folder().await;
    save_draft_explicit_folder_and_errors().await;
    sync_now_runs_and_checks_rights().await;
    endpoint_speaks_mcp_behind_the_token_guard().await;
}

async fn list_accounts_and_folders() {
    let fx = fixture().await;
    let tools = BichonMcpTools::new(admin());
    let accounts = serde_json::to_value(tools.list_accounts().await.unwrap().0).unwrap();
    assert!(accounts
        .as_array()
        .unwrap()
        .iter()
        .any(|a| a["account_id"] == fx.archive_id && a["email"] == "archive@example.com"));
    let folders = serde_json::to_value(tools.list_folders(fx.archive_id).await.unwrap().0).unwrap();
    assert!(folders
        .as_array()
        .unwrap()
        .iter()
        .any(|f| f["name"] == "INBOX"));

    let nobody = BichonMcpTools::new(nobody());
    let visible = nobody.list_accounts().await.unwrap().0;
    assert!(visible.is_empty());
    assert!(nobody
        .list_folders(fx.archive_id)
        .await
        .unwrap_err()
        .contains("Permission"));
}

async fn search_filters_and_errors() {
    let fx = fixture().await;
    let tools = BichonMcpTools::new(admin());
    let hits = search_ids(&admin(), Some("zebra"), Some(fx.archive_id)).await;
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0]["envelope_id"], fx.original_id.as_str());
    assert_eq!(hits[0]["attachments"], 1);

    let page = tools
        .search_emails(
            None,
            None,
            Some("alice".into()),
            None,
            Some("alice@example.org".into()),
            Some("2026-10-01".into()),
            Some("2026-10-07T00:00:00Z".into()),
            Some(vec![fx.archive_id]),
            None,
            Some(true),
            Some("akt".into()),
            Some(1),
            Some(500),
        )
        .await
        .unwrap();
    let page = serde_json::to_value(page.0).unwrap();
    assert_eq!(page["total"], 1, "{page}");
    assert_eq!(page["page"], 1);

    let bad = tools
        .search_emails(
            None,
            None,
            None,
            None,
            None,
            Some("yesterday".into()),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .await;
    assert!(bad.unwrap_err().contains("Bad date"));
    let bad = tools
        .search_emails(
            None,
            None,
            None,
            None,
            None,
            None,
            Some("tomorrow".into()),
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .await;
    assert!(bad.unwrap_err().contains("Bad date"));
}

async fn get_email_and_thread() {
    let fx = fixture().await;
    let tools = BichonMcpTools::new(admin());
    let email = tools
        .get_email(fx.archive_id, format!(" {} ", fx.original_id), None)
        .await
        .unwrap();
    let email = serde_json::to_value(email.0).unwrap();
    assert!(email["text"].as_str().unwrap().contains("zebra-marker"));
    assert!(email.get("html").is_none());
    assert_eq!(email["attachments"][0]["filename"], "akt.pdf");
    assert_eq!(email["thread_id"], fx.thread_id.as_str());

    // HTML-only message: text is converted from the HTML; HTML on request.
    let reply = tools
        .get_email(fx.archive_id, fx.reply_id.clone(), None)
        .await
        .unwrap();
    let reply = serde_json::to_value(reply.0).unwrap();
    assert!(reply["text"].as_str().unwrap().contains("Thanks"));
    assert!(reply.get("html").is_none());
    assert_eq!(reply["cc"][0], "boss@example.org");
    let reply = tools
        .get_email(fx.archive_id, fx.reply_id.clone(), Some(true))
        .await
        .unwrap();
    let reply = serde_json::to_value(reply.0).unwrap();
    assert!(reply["html"].as_str().unwrap().contains("<p>Thanks"));

    let thread = tools
        .get_thread(fx.archive_id, fx.thread_id.clone(), None, None)
        .await
        .unwrap();
    assert_eq!(serde_json::to_value(thread.0).unwrap()["total"], 2);

    assert!(tools
        .get_email(fx.archive_id, "nope".into(), Some(true))
        .await
        .is_err());
    let nobody = BichonMcpTools::new(nobody());
    assert!(nobody
        .get_email(fx.archive_id, fx.original_id.clone(), None)
        .await
        .is_err());
    assert!(nobody
        .get_thread(fx.archive_id, fx.thread_id.clone(), Some(1), Some(10))
        .await
        .unwrap_err()
        .contains("Permission"));
}

async fn save_attachments_writes_files() {
    let fx = fixture().await;
    let tools = BichonMcpTools::new(admin());
    let saved = tools
        .save_attachments(fx.archive_id, fx.original_id.clone(), None)
        .await
        .unwrap();
    let saved = serde_json::to_value(saved.0).unwrap();
    let path = saved[0]["path"].as_str().unwrap();
    assert!(path.contains("attachments/archive@example.com/2026-10-06 "));
    assert!(path.ends_with("akt.pdf"));
    assert_eq!(std::fs::read(path).unwrap(), b"PDFDATA");

    let email = serde_json::to_value(
        tools
            .get_email(fx.archive_id, fx.original_id.clone(), None)
            .await
            .unwrap()
            .0,
    )
    .unwrap();
    let hash = email["attachments"][0]["content_hash"].as_str().unwrap();
    let one = tools
        .save_attachments(fx.archive_id, fx.original_id.clone(), Some(hash.into()))
        .await
        .unwrap();
    assert_eq!(one.0.len(), 1);
    assert!(tools
        .save_attachments(fx.archive_id, fx.original_id.clone(), Some("nope".into()))
        .await
        .unwrap_err()
        .contains("No such attachment"));
    assert!(tools
        .save_attachments(fx.archive_id, fx.reply_id.clone(), None)
        .await
        .is_err());
}

// ── Drafts and sync against the scripted IMAP server ────────────────────────

async fn save_draft_appends_to_drafts_folder() {
    let (account_id, appended) = imap_account("drafts@example.com", true).await;
    let att = save(&["out"], "invoice.pdf", b"%PDF-1").unwrap();
    let tools = BichonMcpTools::new(admin());
    let saved = tools
        .save_draft(
            account_id,
            Some(vec!["client@example.org".into()]),
            None,
            None,
            Some("Invoice".into()),
            Some("Please find attached".into()),
            None,
            None,
            Some(vec![att.display().to_string()]),
            None,
        )
        .await
        .unwrap();
    let saved = serde_json::to_value(saved.0).unwrap();
    assert_eq!(saved["folder"], "Drafts");
    assert_eq!(saved["attachments"][0], "invoice.pdf");

    let drafts = appended.lock().unwrap().clone();
    let (mailbox, flags, raw) = drafts
        .iter()
        .find(|(_, _, raw)| String::from_utf8_lossy(raw).contains("Invoice"))
        .expect("draft appended");
    assert_eq!(mailbox, "Drafts");
    assert_eq!(flags, "(\\Draft \\Seen)");
    let msg = parse(raw);
    assert_eq!(
        msg.from().unwrap().first().unwrap().address(),
        Some("drafts@example.com")
    );
    assert_eq!(
        msg.attachment(0).unwrap().attachment_name(),
        Some("invoice.pdf")
    );
}

async fn save_draft_explicit_folder_and_errors() {
    let (account_id, appended) = imap_account("folders@example.com", false).await;

    // No \Drafts attribute on the server: an explicit, known folder is needed.
    let err = append_draft(account_id, None, b"Subject: x\r\n\r\ny")
        .await
        .unwrap_err();
    assert!(err.contains("no folder as \\Drafts"), "{err}");
    assert!(append_draft(account_id, Some("Missing"), b"x")
        .await
        .unwrap_err()
        .contains("not known"));
    MailBox::batch_upsert(&[MailBox {
        id: account_id ^ 0x5eed,
        account_id,
        name: "Drafts".into(),
        delimiter: Some("/".into()),
        ..Default::default()
    }])
    .unwrap();
    let folder = append_draft(account_id, Some("Drafts"), b"Subject: x\r\n\r\ny")
        .await
        .unwrap();
    assert_eq!(folder, "Drafts");
    assert!(!appended.lock().unwrap().is_empty());

    // Drafts only for IMAP accounts, and only with the permission.
    let fx = fixture().await;
    let local = save_draft(DraftRequest {
        account_id: fx.archive_id,
        ..Default::default()
    })
    .await;
    assert!(local.unwrap_err().contains("only supported for IMAP"));
    assert!(save_draft(DraftRequest {
        account_id: u64::MAX,
        ..Default::default()
    })
    .await
    .unwrap_err()
    .contains("Account"));
    let nobody = BichonMcpTools::new(nobody());
    let denied = nobody
        .save_draft(
            account_id, None, None, None, None, None, None, None, None, None,
        )
        .await;
    assert!(denied.unwrap_err().contains("Permission"));

    // A reply must point at a readable message of the same account.
    let tools = BichonMcpTools::new(admin());
    let missing = tools
        .save_draft(
            account_id,
            None,
            None,
            None,
            None,
            Some("t".into()),
            None,
            Some("nope".into()),
            None,
            None,
        )
        .await;
    assert!(missing.is_err());
    let reply_elsewhere = save_draft(DraftRequest {
        account_id,
        reply_to_envelope_id: Some(fx.original_id.clone()),
        text: Some("t".into()),
        ..Default::default()
    })
    .await;
    assert!(reply_elsewhere.unwrap_err().contains("Original message"));
}

async fn sync_now_runs_and_checks_rights() {
    let (account_id, _) = imap_account("sync@example.com", true).await;
    let tools = BichonMcpTools::new(admin());
    let done = tools.sync_now(account_id, Some(60)).await.unwrap();
    let done = serde_json::to_value(done.0).unwrap();
    assert_eq!(done["account"], "sync@example.com");
    assert!(["finished", "still running"].contains(&done["status"].as_str().unwrap()));

    let fx = fixture().await;
    assert!(tools
        .sync_now(fx.archive_id, None)
        .await
        .unwrap_err()
        .contains("Only IMAP"));
    assert!(tools
        .sync_now(u64::MAX, None)
        .await
        .unwrap_err()
        .contains("Account"));
    assert!(BichonMcpTools::new(nobody())
        .sync_now(account_id, None)
        .await
        .unwrap_err()
        .contains("Permission"));
}

// ── The HTTP endpoint ───────────────────────────────────────────────────────

async fn endpoint_speaks_mcp_behind_the_token_guard() {
    let fx = fixture().await;
    let token = crate::tests::admin_token().await;
    let cli = TestClient::new(Route::new().nest_no_strip("/mcp", mcp_endpoint().with(ApiGuard)))
        .default_header("X-Forwarded-For", "127.0.0.1")
        .default_header("Accept", "application/json, text/event-stream");

    let anonymous = cli.post("/mcp").body_json(&json!({})).send().await;
    assert_eq!(anonymous.0.status(), StatusCode::UNAUTHORIZED);

    let auth = format!("Bearer {token}");
    let init = cli
        .post("/mcp")
        .header("Authorization", &auth)
        .body_json(
            &json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
            "protocolVersion": "2025-06-18", "capabilities": {},
            "clientInfo": {"name": "test", "version": "0"}}}),
        )
        .send()
        .await;
    init.assert_status_is_ok();
    let session = init
        .0
        .headers()
        .get("Mcp-Session-Id")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();

    let note = cli
        .post("/mcp")
        .header("Authorization", &auth)
        .header("Mcp-Session-Id", &session)
        .body_json(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
        .send()
        .await;
    assert_eq!(note.0.status(), StatusCode::ACCEPTED);

    let call = cli
        .post("/mcp")
        .header("Authorization", &auth)
        .header("Mcp-Session-Id", &session)
        .body_json(
            &json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {
            "name": "list_folders", "arguments": {"account_id": fx.archive_id}}}),
        )
        .send()
        .await;
    call.assert_status_is_ok();
    let body: Value =
        serde_json::from_str(&call.0.into_body().into_string().await.unwrap()).unwrap();
    assert_eq!(body["id"], 2, "a single object, not a batch: {body}");
    assert_eq!(body["result"]["isError"], false);
    assert!(body["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("INBOX"));
}
