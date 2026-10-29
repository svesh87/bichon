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

//! The MCP file exchange directory (`BICHON_MCP_FILES_DIR`): the only place
//! the MCP tools write to or read from on the local file system.

use std::path::{Path, PathBuf};

use super::MCP_SETTINGS;

/// Draft attachments larger than this are refused (most providers cap a
/// message at 25 MB, and base64 grows it by a third).
pub const MAX_DRAFT_ATTACHMENTS_BYTES: u64 = 18 * 1024 * 1024;

fn files_dir() -> Result<PathBuf, String> {
    let dir = MCP_SETTINGS
        .files_dir
        .as_ref()
        .ok_or("File exchange is not configured (BICHON_MCP_FILES_DIR is not set)")?;
    dir.canonicalize()
        .map_err(|e| format!("File exchange directory {}: {e}", dir.display()))
}

/// A file name safe to create: no path separators, control characters or
/// leading dots, at most 150 characters.
pub fn safe_file_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    let cleaned = cleaned.trim().trim_start_matches('.').trim();
    let cleaned: String = cleaned.chars().take(150).collect();
    if cleaned.is_empty() {
        "attachment".to_string()
    } else {
        cleaned
    }
}

/// Write `content` to `<files dir>/<subdir>/<name>` through a temporary file
/// and a rename, so that a reader (or a file sync) never sees a partial file.
/// An existing file with the same name is replaced.
pub fn save(subdir: &[&str], name: &str, content: &[u8]) -> Result<PathBuf, String> {
    let mut dir = files_dir()?;
    for part in subdir {
        dir.push(safe_file_name(part));
    }
    std::fs::create_dir_all(&dir).map_err(|e| format!("Create {}: {e}", dir.display()))?;
    let target = dir.join(safe_file_name(name));
    let tmp = dir.join(format!(".{}.part", uuid::Uuid::new_v4()));
    std::fs::write(&tmp, content).map_err(|e| format!("Write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, &target).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("Rename to {}: {e}", target.display())
    })?;
    Ok(target)
}

/// Resolve a path given by the agent to a regular file inside the files
/// directory (symlinks resolved first, so they cannot point outside).
pub fn resolve_input(path: &str) -> Result<PathBuf, String> {
    let dir = files_dir()?;
    let given = Path::new(path);
    let full = if given.is_absolute() {
        given.to_path_buf()
    } else {
        dir.join(given)
    };
    let real = full
        .canonicalize()
        .map_err(|e| format!("Attachment {path}: {e}"))?;
    if !real.starts_with(&dir) {
        return Err(format!(
            "Attachment {path} is outside the file exchange directory {}",
            dir.display()
        ));
    }
    if !real.is_file() {
        return Err(format!("Attachment {path} is not a regular file"));
    }
    Ok(real)
}
