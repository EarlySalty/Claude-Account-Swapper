use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub const HISTORY_SECONDS: u64 = 45 * 60;
const MIN_SPAN_SECONDS: u64 = 10 * 60;

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Sample {
    pub at: u64,
    pub five_hour: f64,
}

pub fn record(history: &mut Vec<Sample>, sample: Sample) {
    if history
        .last()
        .is_some_and(|last| sample.five_hour < last.five_hour)
    {
        history.clear();
    }
    history.retain(|kept| kept.at < sample.at && sample.at - kept.at <= HISTORY_SECONDS);
    history.push(sample);
}

fn recent(history: &[Sample], now: u64) -> impl Iterator<Item = &Sample> {
    history
        .iter()
        .filter(move |sample| now.saturating_sub(sample.at) <= HISTORY_SECONDS)
}

fn growth(history: &[Sample], now: u64) -> Option<(f64, u64)> {
    let first = recent(history, now).next()?;
    let last = recent(history, now).last()?;
    Some((
        (last.five_hour - first.five_hour).max(0.0),
        now.saturating_sub(first.at),
    ))
}

pub fn burn_per_minute<'a>(
    histories: impl IntoIterator<Item = &'a [Sample]>,
    now: u64,
) -> Option<f64> {
    let (grown, span) = histories
        .into_iter()
        .filter_map(|history| growth(history, now))
        .fold((0.0, 0), |(grown, span), (more, longer)| {
            (grown + more, span.max(longer))
        });
    (span >= MIN_SPAN_SECONDS && grown > 0.0).then(|| grown * 60.0 / span as f64)
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Headroom {
    pub left: f64,
    pub refill: f64,
    pub resets_at: Option<DateTime<Utc>>,
}

pub fn minutes_until_exhausted(
    accounts: &[Headroom],
    burn: f64,
    now: DateTime<Utc>,
) -> Option<f64> {
    if burn <= 0.0 {
        return None;
    }
    let mut left: f64 = accounts.iter().map(|account| account.left).sum();
    let mut refills: Vec<(f64, f64)> = accounts
        .iter()
        .filter(|account| account.refill > 0.0)
        .filter_map(|account| {
            let until = account.resets_at?.signed_duration_since(now).num_seconds();
            (until > 0).then_some((until as f64 / 60.0, account.refill))
        })
        .collect();
    refills.sort_by(|a, b| a.0.total_cmp(&b.0));
    for (until, refill) in refills {
        if left / burn <= until {
            break;
        }
        left += refill;
    }
    Some(left / burn)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(at: u64, five_hour: f64) -> Sample {
        Sample { at, five_hour }
    }

    #[test]
    fn record_starts_over_after_a_window_reset_and_forgets_old_samples() {
        let mut history = Vec::new();
        record(&mut history, sample(0, 40.0));
        record(&mut history, sample(600, 50.0));
        record(&mut history, sample(1200, 3.0));
        assert_eq!(history, vec![sample(1200, 3.0)]);
        record(&mut history, sample(1200 + HISTORY_SECONDS + 1, 10.0));
        assert_eq!(history, vec![sample(1200 + HISTORY_SECONDS + 1, 10.0)]);
    }

    #[test]
    fn burn_adds_growth_of_every_account_over_the_longest_span() {
        let active = vec![sample(1300, 60.0), sample(1800, 70.0)];
        let idle = vec![sample(0, 90.0), sample(1800, 90.0)];
        let burn = burn_per_minute([active.as_slice(), idle.as_slice()], 1800).expect("burn");
        assert!((burn - 10.0 / 30.0).abs() < 1e-9);
        assert_eq!(burn_per_minute([active.as_slice()], 1800), None);
        assert_eq!(burn_per_minute([idle.as_slice()], 1800), None);
    }

    #[test]
    fn exhaustion_counts_resets_that_happen_before_the_pool_runs_dry() {
        let now = Utc::now();
        let soon = Some(now + chrono::Duration::minutes(10));
        let late = Some(now + chrono::Duration::minutes(300));
        let accounts = [
            Headroom {
                left: 20.0,
                refill: 75.0,
                resets_at: late,
            },
            Headroom {
                left: 0.0,
                refill: 95.0,
                resets_at: soon,
            },
        ];
        let minutes = minutes_until_exhausted(&accounts, 1.0, now).expect("forecast");
        assert!((minutes - 115.0).abs() < 1e-9);
        let accounts = [
            Headroom {
                left: 5.0,
                refill: 90.0,
                resets_at: late,
            },
            Headroom {
                left: 0.0,
                refill: 95.0,
                resets_at: soon,
            },
        ];
        let minutes = minutes_until_exhausted(&accounts, 1.0, now).expect("forecast");
        assert!((minutes - 5.0).abs() < 1e-9);
        assert_eq!(minutes_until_exhausted(&accounts, 0.0, now), None);
    }
}
