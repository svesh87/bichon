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

//! MCP (Model Context Protocol) endpoint for AI agents (svesh fork).
//!
//! Based on upstream's unfinished `feature/add-mcp` branch (602bc22), ported to
//! 2.x and extended with on-demand sync, attachment export and IMAP drafts.
//! Configured by its own environment variables so the upstream settings stay
//! untouched:
//!
//! - `BICHON_ENABLE_MCP=true` mounts `/mcp` (behind `ApiGuard`).
//! - `BICHON_MCP_FILES_DIR=/path` is the file exchange directory: attachments
//!   are saved under it, and draft attachments may only be read from it.

use std::{path::PathBuf, sync::Arc, sync::LazyLock};

use bichon_core::common::auth::ClientContext;
use poem::{http::StatusCode, Body, Endpoint, EndpointExt, IntoResponse, Request, Response};
use poem_mcpserver::{streamable_http, McpServer};

mod draft;
mod files;
#[cfg(test)]
mod tests;
mod tools;
use tools::BichonMcpTools;

pub struct McpSettings {
    pub enabled: bool,
    pub files_dir: Option<PathBuf>,
}

pub static MCP_SETTINGS: LazyLock<McpSettings> = LazyLock::new(|| McpSettings {
    enabled: std::env::var("BICHON_ENABLE_MCP")
        .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
        .unwrap_or(false),
    files_dir: std::env::var_os("BICHON_MCP_FILES_DIR")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from),
});

/// Create a Poem endpoint that handles MCP Streamable HTTP requests.
///
/// The endpoint requires authentication via `ApiGuard` middleware (applied at
/// the route level in `rest/mod.rs`). The guard stores the `ClientContext` in
/// the request data; every tool checks the caller's permissions with it.
pub fn mcp_endpoint() -> impl poem::IntoEndpoint {
    streamable_http::endpoint(|req: &Request| {
        let ctx = req
            .data::<Arc<ClientContext>>()
            .expect("ApiGuard middleware must provide ClientContext in request data")
            .as_ref()
            .clone();
        McpServer::new()
            .with_server_info("bichon-mcp", env!("CARGO_PKG_VERSION"))
            .tools(BichonMcpTools::new(ctx))
    })
    .around(unbatch)
}

/// poem-mcpserver answers every JSON POST with an array, also for a single
/// request and for notifications (`[]`). MCP 2025-06-18 has no batches: a
/// request gets one response object, a notification `202 Accepted`.
async fn unbatch<E: Endpoint>(ep: Arc<E>, req: Request) -> poem::Result<Response> {
    let resp = ep.call(req).await?.into_response();
    let is_json = resp
        .content_type()
        .is_some_and(|ct| ct.starts_with("application/json"));
    if !is_json || !resp.status().is_success() {
        return Ok(resp);
    }
    let (mut parts, body) = resp.into_parts();
    let bytes = body.into_bytes().await?;
    let bytes = match serde_json::from_slice::<serde_json::Value>(&bytes) {
        Ok(serde_json::Value::Array(items)) if items.is_empty() => {
            return Ok(Response::builder().status(StatusCode::ACCEPTED).finish());
        }
        Ok(serde_json::Value::Array(mut items)) if items.len() == 1 => {
            serde_json::to_vec(&items.remove(0)).unwrap_or_default()
        }
        _ => bytes.to_vec(),
    };
    parts.headers.remove(http::header::CONTENT_LENGTH);
    Ok(Response::from_parts(parts, Body::from(bytes)))
}
