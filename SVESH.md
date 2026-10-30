# svesh fork of Bichon

Branch `svesh` = an upstream release tag + the commits below. `main` mirrors upstream.

<!-- renovate: datasource=github-tags depName=rustmailer/bichon -->
Upstream: 2.2.0

## What the fork adds

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
- **Folders without new mail are skipped** (`archive/imap/download/unchanged.rs`): when a
  folder's UIDNEXT from the round's STATUS has not moved past the archived watermark, its
  incremental sync opens no connection.
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
| `BICHON_IMAP_FETCH_CHUNK_KB` | chunked body download, part size in KB (4–65536; Thunderbird uses 136); unset or 0 — off, upstream behaviour. Global, for every account |

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
