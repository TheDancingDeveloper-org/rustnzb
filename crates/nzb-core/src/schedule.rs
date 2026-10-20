//! Time-of-day download schedule.
//!
//! A rule names a set of weekdays and a window, and what should happen inside
//! it: pause downloads, cap the speed, or take news servers out of rotation.
//! Windows may cross midnight; such a window belongs to the day it starts on.
//!
//! Evaluation is a pure function of the rules and a wall-clock time, so the
//! behaviour around midnight and at window edges is testable without a clock,
//! a timer or a running queue.

use chrono::{Datelike, NaiveDate, NaiveDateTime, NaiveTime, Timelike, Weekday};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// What an active rule does to the queue.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ScheduleAction {
    /// Pause all downloads.
    #[default]
    Pause,
    /// Apply a speed limit for the window.
    SpeedLimit,
    /// Take the listed servers out of rotation.
    DisableServers,
}

/// The whole `[schedule]` section.
#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ScheduleConfig {
    /// Rules are only evaluated when this is on. Turning it off restores the
    /// configured speed limit, resumes a pause the schedule applied, and
    /// returns every server to rotation.
    pub enabled: bool,
    pub rules: Vec<ScheduleRule>,
}

/// One time-of-day window.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ScheduleRule {
    /// Stable identifier, assigned by the API when a rule is created.
    pub id: String,
    /// Label shown in the UI.
    pub name: String,
    /// Weekdays the window starts on: `mon`..`sun` (case-insensitive, full
    /// names are accepted too).
    pub days: Vec<String>,
    /// Window start as `HH:MM`, 24-hour clock.
    pub start: String,
    /// Window end as `HH:MM`. Earlier than `start` means the window runs past
    /// midnight. `24:00` is accepted as the end of the day.
    pub end: String,
    pub action: ScheduleAction,
    /// Speed cap in bytes/sec for `speed_limit`; `0` means unlimited, which
    /// explicitly overrides a lower cap from another active rule.
    pub speed_limit_bps: u64,
    /// Server ids taken out of rotation for `disable_servers`.
    pub servers: Vec<String>,
}

impl Default for ScheduleRule {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: String::new(),
            days: Vec::new(),
            start: "00:00".into(),
            end: "24:00".into(),
            action: ScheduleAction::Pause,
            speed_limit_bps: 0,
            servers: Vec::new(),
        }
    }
}

/// What the schedule wants right now.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScheduleDecision {
    /// Rules active at the evaluated time, in configuration order.
    pub active_rule_ids: Vec<String>,
    /// Whether downloads should be paused. Any active pause rule pauses.
    pub pause: bool,
    /// Effective scheduled cap. `None` means no scheduled cap, so the
    /// configured `general.speed_limit_bps` applies. `Some(0)` means an
    /// active rule asked for unlimited speed, which overrides both the
    /// configured limit and any lower cap from another rule.
    pub speed_limit_bps: Option<u64>,
    /// Server ids to take out of rotation, in configuration order and without
    /// duplicates.
    pub disabled_servers: Vec<String>,
    /// Every open pause window *occurrence*, as `rule id@the date the window
    /// started on`, sorted and without duplicates.
    ///
    /// The occurrences, not a single joined key, are what the queue compares:
    /// with two windows overlapping, one closing must not look like a new
    /// window opening. Two evaluations a tick apart inside one window share
    /// their occurrence, while the next day's window (or a window the user
    /// reopened after a manual resume) has a different one.
    pub pause_windows: Vec<String>,
}

impl ScheduleDecision {
    /// Nothing to do: no pause, no cap and every server enabled.
    pub fn is_idle(&self) -> bool {
        !self.pause
            && self.speed_limit_bps.is_none()
            && self.disabled_servers.is_empty()
            && self.pause_windows.is_empty()
    }
}

/// Parse a weekday name.
pub fn parse_weekday(name: &str) -> Option<Weekday> {
    let name = name.trim().to_ascii_lowercase();
    Some(match name.as_str() {
        "mon" | "monday" => Weekday::Mon,
        "tue" | "tues" | "tuesday" => Weekday::Tue,
        "wed" | "weds" | "wednesday" => Weekday::Wed,
        "thu" | "thur" | "thurs" | "thursday" => Weekday::Thu,
        "fri" | "friday" => Weekday::Fri,
        "sat" | "saturday" => Weekday::Sat,
        "sun" | "sunday" => Weekday::Sun,
        _ => return None,
    })
}

/// Parse `HH:MM` into minutes since midnight. `24:00` is accepted as 1440.
pub fn parse_minutes(value: &str) -> Option<u32> {
    let (hour, minute) = value.trim().split_once(':')?;
    if hour.len() != 2 || minute.len() != 2 {
        return None;
    }
    let hour: u32 = hour.parse().ok()?;
    let minute: u32 = minute.parse().ok()?;
    if minute > 59 {
        return None;
    }
    match (hour, minute) {
        (24, 0) => Some(24 * 60),
        (0..=23, _) => Some(hour * 60 + minute),
        _ => None,
    }
}

/// Why a rule cannot be evaluated, phrased for the settings page.
pub fn rule_problem(rule: &ScheduleRule) -> Option<String> {
    if rule.days.is_empty() {
        return Some("pick at least one day".into());
    }
    if let Some(day) = rule.days.iter().find(|day| parse_weekday(day).is_none()) {
        return Some(format!("`{day}` is not a weekday"));
    }
    let Some(start) = parse_minutes(&rule.start) else {
        return Some(format!("start `{}` is not HH:MM", rule.start));
    };
    let Some(end) = parse_minutes(&rule.end) else {
        return Some(format!("end `{}` is not HH:MM", rule.end));
    };
    if start >= 24 * 60 {
        return Some("start must be earlier than 24:00".into());
    }
    if start == end {
        return Some("start and end must differ".into());
    }
    if rule.action == ScheduleAction::DisableServers && rule.servers.is_empty() {
        return Some("pick at least one server".into());
    }
    None
}

/// The day a rule's window started on, when the rule is active at `now`.
///
/// `None` means the rule is not active. For a window that crossed midnight
/// the tail belongs to the previous day, so this is the date the window
/// started, not the date being evaluated.
fn active_window_start(rule: &ScheduleRule, now: NaiveDateTime) -> Option<NaiveDate> {
    let (start, end) = (parse_minutes(&rule.start)?, parse_minutes(&rule.end)?);
    let days: Vec<Weekday> = rule
        .days
        .iter()
        .filter_map(|day| parse_weekday(day))
        .collect();
    let minute = now.hour() * 60 + now.minute();

    if start < end {
        return (days.contains(&now.weekday()) && minute >= start && minute < end)
            .then_some(now.date());
    }

    // The window runs past midnight. The day it belongs to is the start day,
    // so the tail belongs to the previous day's entry.
    if days.contains(&now.weekday()) && minute >= start {
        return Some(now.date());
    }
    now.date()
        .pred_opt()
        .filter(|yesterday| days.contains(&yesterday.weekday()) && minute < end)
}

/// Evaluate every rule against `now` (local wall-clock time).
///
/// Invalid rules are skipped: they cannot be saved through the API, and a
/// hand-edited file with one must not take the queue down with it.
pub fn evaluate(rules: &[ScheduleRule], now: NaiveDateTime) -> ScheduleDecision {
    let mut decision = ScheduleDecision::default();
    let mut saw_unlimited = false;
    let mut pause_windows: Vec<String> = Vec::new();

    for rule in rules {
        if rule_problem(rule).is_some() {
            continue;
        }
        let Some(window_start) = active_window_start(rule, now) else {
            continue;
        };
        decision.active_rule_ids.push(rule.id.clone());
        match rule.action {
            ScheduleAction::Pause => {
                decision.pause = true;
                pause_windows.push(format!("{}@{}", rule.id, window_start));
            }
            ScheduleAction::SpeedLimit => {
                if rule.speed_limit_bps == 0 {
                    saw_unlimited = true;
                } else {
                    decision.speed_limit_bps = Some(
                        decision
                            .speed_limit_bps
                            .map_or(rule.speed_limit_bps, |bps| bps.min(rule.speed_limit_bps)),
                    );
                }
            }
            ScheduleAction::DisableServers => {
                for server in &rule.servers {
                    if !decision.disabled_servers.contains(server) {
                        decision.disabled_servers.push(server.clone());
                    }
                }
            }
        }
    }

    // An active rule that says "unlimited" wins over a cap: it is an explicit
    // statement about this window, not the absence of one. It has to be
    // `Some(0)`, not `None` -- `None` means "no scheduled cap", which would
    // let the configured `general.speed_limit_bps` through and run the window
    // at the user's limit instead of unlimited.
    if saw_unlimited {
        decision.speed_limit_bps = Some(0);
    }
    // Sorted so the occurrences do not depend on config order, which the
    // settings page may reorder without changing what is open.
    pause_windows.sort();
    pause_windows.dedup();
    decision.pause_windows = pause_windows;
    decision
}

/// Evaluate a whole config: a disabled schedule is always idle.
pub fn evaluate_config(config: &ScheduleConfig, now: NaiveDateTime) -> ScheduleDecision {
    if !config.enabled {
        return ScheduleDecision::default();
    }
    evaluate(&config.rules, now)
}

/// The next minute boundary after `now`, used to align the monitor's wakeups.
pub fn next_minute(now: NaiveTime) -> NaiveTime {
    let minute = now.hour() * 60 + now.minute();
    let next = (minute + 1) % (24 * 60);
    NaiveTime::from_hms_opt(next / 60, next % 60, 0).expect("valid minute of day")
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    fn at(day: u32, hour: u32, minute: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(2026, 10, day)
            .unwrap()
            .and_hms_opt(hour, minute, 0)
            .unwrap()
    }

    fn rule(days: &[&str], start: &str, end: &str, action: ScheduleAction) -> ScheduleRule {
        ScheduleRule {
            id: format!("{start}-{end}"),
            name: String::new(),
            days: days.iter().map(|d| (*d).to_string()).collect(),
            start: start.into(),
            end: end.into(),
            action,
            ..ScheduleRule::default()
        }
    }

    #[test]
    fn parses_clock_times_and_rejects_bad_ones() {
        assert_eq!(parse_minutes("00:00"), Some(0));
        assert_eq!(parse_minutes("09:05"), Some(545));
        assert_eq!(parse_minutes("23:59"), Some(1439));
        assert_eq!(parse_minutes("24:00"), Some(1440));
        assert_eq!(parse_minutes("24:01"), None);
        assert_eq!(parse_minutes("7:00"), None);
        assert_eq!(parse_minutes("07:0"), None);
        assert_eq!(parse_minutes("07:60"), None);
        assert_eq!(parse_minutes("noon"), None);
    }

    #[test]
    fn parses_weekday_names_case_insensitively() {
        assert_eq!(parse_weekday("Mon"), Some(Weekday::Mon));
        assert_eq!(parse_weekday("thursday"), Some(Weekday::Thu));
        assert_eq!(parse_weekday("sunday "), Some(Weekday::Sun));
        assert_eq!(parse_weekday("funday"), None);
    }

    #[test]
    fn a_daytime_window_covers_its_start_and_excludes_its_end() {
        let rules = [rule(&["tue"], "09:00", "17:00", ScheduleAction::Pause)];
        // 2026-10-20 is a Tuesday.
        assert!(!evaluate(&rules, at(20, 8, 59)).pause);
        assert!(evaluate(&rules, at(20, 9, 0)).pause);
        assert!(evaluate(&rules, at(20, 16, 59)).pause);
        assert!(!evaluate(&rules, at(20, 17, 0)).pause);
        assert!(!evaluate(&rules, at(21, 12, 0)).pause, "wrong weekday");
    }

    #[test]
    fn an_overnight_window_belongs_to_the_day_it_starts_on() {
        let rules = [rule(&["mon"], "22:00", "06:00", ScheduleAction::Pause)];
        // Monday 2026-10-19 at 23:00 is inside; Tuesday 05:00 is the tail.
        assert!(evaluate(&rules, at(19, 23, 0)).pause);
        assert!(evaluate(&rules, at(20, 5, 59)).pause);
        assert!(!evaluate(&rules, at(20, 6, 0)).pause, "tail ends at 06:00");
        assert!(
            !evaluate(&rules, at(20, 22, 30)).pause,
            "Tuesday starts nothing"
        );
        assert!(!evaluate(&rules, at(19, 21, 59)).pause);
    }

    #[test]
    fn a_full_day_window_is_active_all_day() {
        let rules = [rule(&["sat"], "00:00", "24:00", ScheduleAction::Pause)];
        assert!(evaluate(&rules, at(24, 0, 0)).pause);
        assert!(evaluate(&rules, at(24, 23, 59)).pause);
        assert!(!evaluate(&rules, at(25, 0, 0)).pause);
    }

    #[test]
    fn the_lowest_applied_cap_wins_and_unlimited_overrides_one() {
        let mut off_peak = rule(&["tue"], "00:00", "24:00", ScheduleAction::SpeedLimit);
        off_peak.id = "off-peak".into();
        off_peak.speed_limit_bps = 1_000_000;
        let mut strict = rule(&["tue"], "00:00", "24:00", ScheduleAction::SpeedLimit);
        strict.id = "strict".into();
        strict.speed_limit_bps = 500_000;

        let decision = evaluate(&[off_peak.clone(), strict.clone()], at(20, 12, 0));
        assert_eq!(decision.speed_limit_bps, Some(500_000));
        assert_eq!(decision.active_rule_ids, vec!["off-peak", "strict"]);

        let mut unlimited = rule(&["tue"], "00:00", "24:00", ScheduleAction::SpeedLimit);
        unlimited.id = "unlimited".into();
        let decision = evaluate(&[off_peak, strict, unlimited], at(20, 12, 0));
        assert_eq!(
            decision.speed_limit_bps,
            Some(0),
            "unlimited is an explicit 0 cap, not the absence of one"
        );
    }

    #[test]
    fn an_unlimited_rule_is_distinct_from_having_no_cap_rule() {
        let mut unlimited = rule(&["tue"], "00:00", "24:00", ScheduleAction::SpeedLimit);
        unlimited.id = "unlimited".into();
        assert_eq!(
            evaluate(&[unlimited], at(20, 12, 0)).speed_limit_bps,
            Some(0)
        );
        // With no cap rule at all the configured limit must still apply, so
        // this is the case that has to stay `None`.
        assert_eq!(evaluate(&[], at(20, 12, 0)).speed_limit_bps, None);
    }

    #[test]
    fn a_pause_window_carries_an_occurrence_that_changes_with_the_window() {
        let rules = [
            rule(&["mon"], "22:00", "06:00", ScheduleAction::Pause),
            rule(&["tue"], "22:00", "06:00", ScheduleAction::Pause),
        ];
        // Inside Monday's window, before and after midnight: the same
        // occurrence, because it belongs to the day it started on.
        let monday_late = evaluate(&rules[..1], at(19, 23, 0)).pause_windows;
        let tuesday_early = evaluate(&rules[..1], at(20, 5, 0)).pause_windows;
        assert_eq!(monday_late, tuesday_early);
        assert_eq!(monday_late.len(), 1);
        assert!(monday_late[0].contains("2026-10-19"), "{monday_late:?}");

        // Tuesday's own window is a different occurrence, so the queue sees
        // it as a new window and pauses again.
        let tuesday_late = evaluate(&rules[1..], at(20, 23, 0)).pause_windows;
        assert_ne!(monday_late, tuesday_late);
        assert!(tuesday_late[0].contains("2026-10-20"), "{tuesday_late:?}");

        // Outside every window there is no occurrence at all.
        assert!(evaluate(&rules, at(20, 12, 0)).pause_windows.is_empty());
        // A cap rule opens no pause window.
        let mut cap = rule(&["tue"], "00:00", "24:00", ScheduleAction::SpeedLimit);
        cap.speed_limit_bps = 1_000_000;
        assert!(evaluate(&[cap], at(20, 12, 0)).pause_windows.is_empty());
    }

    #[test]
    fn overlapping_windows_keep_their_own_occurrences() {
        // A 20:00-24:00 and B 22:00-02:00: at 23:00 both are open and each
        // contributes its own occurrence. At 01:00 only B is left, so B must
        // still be in the new set for the queue to recognise it as the same
        // window rather than a new one.
        let mut a = rule(&["mon"], "20:00", "24:00", ScheduleAction::Pause);
        a.id = "a".into();
        let mut b = rule(&["mon"], "22:00", "02:00", ScheduleAction::Pause);
        b.id = "b".into();
        let rules = [a, b];

        let both = evaluate(&rules, at(19, 23, 0)).pause_windows;
        assert_eq!(both, vec!["a@2026-10-19", "b@2026-10-19"]);
        let after_a_closes = evaluate(&rules, at(20, 1, 0)).pause_windows;
        assert_eq!(after_a_closes, vec!["b@2026-10-19"]);
        // The occurrence that is still open is unchanged: a set difference
        // says nothing new opened, so the queue keeps its pause rather than
        // re-pausing a manual resume.
        assert!(
            after_a_closes.iter().all(|window| both.contains(window)),
            "{after_a_closes:?} vs {both:?}"
        );
    }

    #[test]
    fn the_window_occurrence_does_not_depend_on_rule_order() {
        let mut first = rule(&["tue"], "00:00", "24:00", ScheduleAction::Pause);
        first.id = "a".into();
        let mut second = rule(&["tue"], "00:00", "24:00", ScheduleAction::Pause);
        second.id = "b".into();
        // Reordering the rules in settings must not look like a new window.
        assert_eq!(
            evaluate(&[first.clone(), second.clone()], at(20, 12, 0)).pause_windows,
            evaluate(&[second, first], at(20, 12, 0)).pause_windows
        );
    }

    #[test]
    fn a_cap_outside_its_window_does_not_apply() {
        let mut cap = rule(&["tue"], "01:00", "05:00", ScheduleAction::SpeedLimit);
        cap.speed_limit_bps = 1_000_000;
        assert_eq!(evaluate(&[cap], at(20, 12, 0)).speed_limit_bps, None);
    }

    #[test]
    fn disabled_servers_are_collected_once_in_rule_order() {
        let mut first = rule(&["tue"], "00:00", "24:00", ScheduleAction::DisableServers);
        first.id = "a".into();
        first.servers = vec!["backup".into(), "primary".into()];
        let mut second = rule(&["tue"], "00:00", "24:00", ScheduleAction::DisableServers);
        second.id = "b".into();
        second.servers = vec!["backup".into()];

        let decision = evaluate(&[first, second], at(20, 12, 0));
        assert_eq!(decision.disabled_servers, vec!["backup", "primary"]);
    }

    #[test]
    fn a_disabled_schedule_is_idle_whatever_the_clock_says() {
        let config = ScheduleConfig {
            enabled: false,
            rules: vec![rule(&["tue"], "00:00", "24:00", ScheduleAction::Pause)],
        };
        assert!(evaluate_config(&config, at(20, 12, 0)).is_idle());
        let config = ScheduleConfig {
            enabled: true,
            ..config
        };
        assert!(!evaluate_config(&config, at(20, 12, 0)).is_idle());
    }

    #[test]
    fn invalid_rules_are_skipped_rather_than_applied() {
        let broken = ScheduleRule {
            id: "broken".into(),
            days: vec!["mon".into()],
            start: "nope".into(),
            end: "06:00".into(),
            ..ScheduleRule::default()
        };
        assert!(evaluate(&[broken], at(19, 23, 0)).is_idle());
    }

    #[test]
    fn validation_names_the_field_at_fault() {
        let mut bad_day = rule(&["funday"], "09:00", "17:00", ScheduleAction::Pause);
        assert!(rule_problem(&bad_day).unwrap().contains("funday"));
        bad_day.days = vec!["mon".into()];
        assert!(rule_problem(&bad_day).is_none());

        let mut bad_time = bad_day.clone();
        bad_time.start = "9:00".into();
        assert!(rule_problem(&bad_time).unwrap().contains("HH:MM"));

        let mut empty = bad_day.clone();
        empty.days.clear();
        assert!(rule_problem(&empty).unwrap().contains("at least one day"));

        let mut same = bad_day.clone();
        same.end = "09:00".into();
        assert!(rule_problem(&same).unwrap().contains("must differ"));

        let mut no_servers = bad_day.clone();
        no_servers.action = ScheduleAction::DisableServers;
        assert!(rule_problem(&no_servers).unwrap().contains("server"));
    }

    #[test]
    fn next_minute_wraps_at_midnight() {
        assert_eq!(
            next_minute(NaiveTime::from_hms_opt(10, 15, 30).unwrap()),
            NaiveTime::from_hms_opt(10, 16, 0).unwrap()
        );
        assert_eq!(
            next_minute(NaiveTime::from_hms_opt(23, 59, 10).unwrap()),
            NaiveTime::from_hms_opt(0, 0, 0).unwrap()
        );
    }
}
