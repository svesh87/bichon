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

use std::time::Duration;

use super::*;

#[test]
fn delays_parse_with_default() {
    assert_eq!(parse_delays(None), vec![30, 60, 120]);
    assert_eq!(parse_delays(Some("")), vec![30, 60, 120]);
    assert_eq!(parse_delays(Some("x,1")), vec![30, 60, 120]);
    assert_eq!(parse_delays(Some(" 5, 10 ")), vec![5, 10]);
    assert_eq!(parse_delays(Some("0")), vec![0]);
}

#[test]
fn no_delay_outside_the_gentle_mode() {
    assert_eq!(delay_if(false, 1, &[30]), Duration::ZERO);
    assert_eq!(delay_if(true, 1, &[30]), Duration::from_secs(30));
}

#[test]
fn delay_per_attempt_repeats_the_last() {
    let delays = [5, 10];
    assert_eq!(delay_for(0, &delays), Duration::from_secs(5));
    assert_eq!(delay_for(1, &delays), Duration::from_secs(5));
    assert_eq!(delay_for(2, &delays), Duration::from_secs(10));
    assert_eq!(delay_for(9, &delays), Duration::from_secs(10));
    assert_eq!(delay_for(1, &[]), Duration::ZERO);
}

#[tokio::test]
async fn wait_sleeps_the_given_delay() {
    let started = std::time::Instant::now();
    wait(1, 1, Duration::ZERO).await;
    assert!(started.elapsed() < Duration::from_millis(20));
    wait(1, 2, Duration::from_millis(30)).await;
    assert!(started.elapsed() >= Duration::from_millis(30));
}

#[test]
fn server_delays_have_a_value_per_attempt() {
    assert!(!RECONNECT_DELAYS.is_empty());
}
