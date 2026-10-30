# svesh fork of Bichon

Branch `svesh` = an upstream release tag + the commits below. `main` mirrors upstream.

<!-- renovate: datasource=github-tags depName=rustmailer/bichon -->
Upstream: 2.2.0

## What the fork adds

The sync changes below form one **gentle mode for slow servers**, on only for accounts with
a "Chunk size" (or with `BICHON_IMAP_FETCH_CHUNK_KB` as the server default). Every other
account syncs exactly as upstream: batches of the account's batch size, no chunks, header
comparison in gap-fill, immediate reconnects.

- **MCP for AI agents** at `/mcp` (streamable HTTP, behind the API token guard), based on
  upstream's unfinished `feature/add-mcp` branch: `list_accounts`, `list_folders`,
  `sync_now`, `search_emails`, `get_email`, `get_thread`, `save_attachments`, `save_draft`.
  Nothing sends, deletes or moves mail. Code: `crates/server/src/mcp/`.
- **Drafts**: `save_draft` builds the message (reply threading, attachments) and APPENDs it
  to the account's `\Drafts` folder with `\Draft \Seen`.
- **Permissions** `account:sync` and `data:draft` (account roles), so an agent token gets
  read + sync + drafts without `account:manage`.
- **Chunked body download** (opt-in, `crates/core/src/imap/chunked.rs`): each message on
  its own in parts of `BODY.PEEK[]<offset.N>`, as Thunderbird does, instead of whole bodies
  of a batch in one FETCH. For servers that stall or drop the connection on large responses
  (Yandex). Applies to sync and to the body pass of gap-fill; not to gap-fill's header scan.
  Messages no larger than a part share one FETCH (up to 1 MB / 50 messages), so ordinary
  mail costs a few round trips per batch; only larger ones go alone, in parts.
  Per account: "Chunk size" in the account's download settings (`imap_fetch_chunk_kb`;
  empty = server default, `0` = off); server default: `BICHON_IMAP_FETCH_CHUNK_KB`.
- **Folders without new mail are skipped** (`archive/imap/download/unchanged.rs`): when a
  folder's UIDNEXT from the round's STATUS has not moved past the archived watermark, its
  incremental sync opens no connection.
- **Gap-fill by UID** (same file): with an unchanged UIDVALIDITY the UIDs not archived in
  the folder are the missing ones and are downloaded right away, without the header scan
  (upstream fetches `HEADER.FIELDS (MESSAGE-ID)` of every message; Yandex answers each batch
  in seconds, hours per folder). A changed UIDVALIDITY keeps the full header comparison.
- **Wait before reconnecting** (`crates/core/src/imap/reconnect.rs`): after a dropped
  connection every retry loop waits before the new LOGIN (upstream reconnected at once and
  waited afterwards; Yandex drops a LOGIN right after a drop).
- **Image** `ghcr.io/svesh87/bichon:<upstream>-<run>`: built from source
  (`docker/svesh.Dockerfile`), runs as uid 1000, by `.github/workflows/svesh-build.yml`.

## Checks

`.github/scripts/svesh-check.sh` (CI job `check`, before the image is built): rustfmt and
clippy without warnings in the fork's code, the fork's tests, line coverage of the fork's
files (MCP, chunked download, folder skip; test files excluded) at least 80%. The MCP tests
run the tools against a temporary store with imported messages and a scripted IMAP server
(drafts, sync); the download tests use upstream's scripted IMAP server. Upstream code is not
judged: it carries warnings of its own.

## Settings

| Variable | Meaning |
|---|---|
| `BICHON_ENABLE_MCP=true` | mount `/mcp` |
| `BICHON_MCP_FILES_DIR` | file exchange directory: attachments are saved under `attachments/<account>/<date subject>/`, draft attachments are read only from inside it |
| `BICHON_LOG_FILTER` | per-target log levels, `RUST_LOG` syntax (e.g. `info,poem_mcpserver=warn`: the MCP library logs whole requests and responses at info); overrides `BICHON_LOG_LEVEL` |
| `BICHON_IMAP_RECONNECT_DELAYS` | seconds to wait before each reconnect after a dropped connection, comma-separated (default `30,60,120`, the last repeats; `0` = no wait) |
| `BICHON_IMAP_FETCH_CHUNK_KB` | chunked body download, part size in KB (4–65536; Thunderbird uses 136); unset or 0 — off, upstream behaviour. Default for accounts without their own "Chunk size" |

## Agent access

Create a user with an account role holding `data:read`, `account:sync`, `data:draft` for
the accounts the agent may use, issue an API token for it and send it as
`Authorization: Bearer <token>`.

## Updating to a new upstream release

```
git fetch upstream --tags
git switch svesh
git merge <tag>          # fork changes are mostly new files: conflicts stay small
# bump "Upstream:" above, build, push
```
