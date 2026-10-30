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

//! svesh: wait before reconnecting after a dropped connection.
//!
//! The retry loops reconnected at once and only then backed off. Right after
//! dropping a connection Yandex also drops the next LOGIN (TLS closed without
//! close_notify), so the immediate reconnect failed as "authentication
//! failed" and the batch or folder was given up. Waiting before the new
//! LOGIN lets the server calm down.
//!
//! Seconds per attempt, comma-separated (the last value repeats; `0` = no
//! wait): the account's "reconnect delays" setting, else the server default
//! `BICHON_IMAP_RECONNECT_DELAYS`, else `30,60,120`. Gentle mode only.

use std::{sync::LazyLock, time::Duration};

const DEFAULT_DELAYS: &[u64] = &[30, 60, 120];

pub static RECONNECT_DELAYS: LazyLock<Vec<u64>> = LazyLock::new(|| {
    parse_delays(
        std::env::var("BICHON_IMAP_RECONNECT_DELAYS")
            .ok()
            .as_deref(),
    )
});

/// Comma-separated seconds; unset or unparsable = the default.
pub fn parse_delays(value: Option<&str>) -> Vec<u64> {
    let parsed: Option<Vec<u64>> = value.map(|v| {
        v.split(',')
            .map(|part| part.trim().parse::<u64>())
            .collect::<Result<Vec<_>, _>>()
            .unwrap_or_default()
    });
    match parsed {
        Some(delays) if !delays.is_empty() => delays,
        _ => DEFAULT_DELAYS.to_vec(),
    }
}

/// Delay before reconnect attempt `attempt` (1-based).
pub fn delay_for(attempt: u32, delays: &[u64]) -> Duration {
    let index = (attempt.max(1) as usize - 1).min(delays.len().saturating_sub(1));
    Duration::from_secs(delays.get(index).copied().unwrap_or(0))
}

/// Sleep before reconnect attempt `attempt` (1-based), for accounts in the
/// gentle mode (a chunk size set, `crate::imap::chunked`); others reconnect
/// at once, as upstream does.
pub async fn pause_before_reconnect(account_id: u64, attempt: u32) {
    let (gentle, delays) = match crate::account::migration::AccountModel::get(account_id) {
        Ok(account) => (
            crate::imap::chunked::is_gentle(&account),
            account_delays(account.imap_reconnect_delays.as_deref()),
        ),
        Err(_) => (false, Vec::new()),
    };
    wait(account_id, attempt, delay_if(gentle, attempt, &delays)).await;
}

/// The account's own delays if set and valid, else the server default.
pub fn account_delays(setting: Option<&str>) -> Vec<u64> {
    match setting.map(str::trim).filter(|s| !s.is_empty()) {
        Some(own) => parse_delays(Some(own)),
        None => RECONNECT_DELAYS.clone(),
    }
}

/// The delay for attempt `attempt`, or none outside the gentle mode.
fn delay_if(gentle: bool, attempt: u32, delays: &[u64]) -> Duration {
    if gentle {
        delay_for(attempt, delays)
    } else {
        Duration::ZERO
    }
}

async fn wait(account_id: u64, attempt: u32, delay: Duration) {
    if delay.is_zero() {
        return;
    }
    tracing::warn!(
        account_id,
        attempt,
        delay_secs = delay.as_secs(),
        "Waiting before reconnecting after a dropped connection"
    );
    tokio::time::sleep(delay).await;
}

#[cfg(test)]
#[path = "reconnect_tests.rs"]
mod tests;
