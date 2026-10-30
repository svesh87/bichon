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

//! svesh: per-target log filter.
//!
//! `BICHON_LOG_LEVEL` sets one level for everything. `BICHON_LOG_FILTER`, when
//! set, takes `RUST_LOG`-style directives instead, e.g.
//! `info,poem_mcpserver=warn`: sync progress at info while the MCP library,
//! which logs whole requests and responses (mail) at info, stays quiet.

use tracing::Level;
use tracing_subscriber::EnvFilter;

/// The filter for `BICHON_LOG_FILTER` (if set and valid), else `level`.
pub fn env_filter(level: Level) -> EnvFilter {
    build(std::env::var("BICHON_LOG_FILTER").ok().as_deref(), level)
}

pub fn build(directives: Option<&str>, level: Level) -> EnvFilter {
    match directives.map(str::trim).filter(|d| !d.is_empty()) {
        Some(directives) => EnvFilter::try_new(directives).unwrap_or_else(|e| {
            eprintln!("Invalid BICHON_LOG_FILTER {directives:?} ({e}); using level {level}");
            EnvFilter::new(level.as_str())
        }),
        None => EnvFilter::new(level.as_str()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn directives_or_level() {
        assert_eq!(build(None, Level::WARN).to_string(), "warn");
        assert_eq!(build(Some("  "), Level::INFO).to_string(), "info");
        assert_eq!(
            build(Some("info,poem_mcpserver=warn"), Level::WARN).to_string(),
            "poem_mcpserver=warn,info"
        );
        assert_eq!(build(Some("info,=[="), Level::ERROR).to_string(), "error");
    }
}
