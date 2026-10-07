use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Result, ensure};
use clap::ValueEnum;
use serde::{Deserialize, Serialize};

use crate::usage::{Bucket, Stops, Usage};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum Strategy {
    #[default]
    Best,
    ConsumeFirst,
    Balanced,
    NextAvailable,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Rule {
    pub account: Option<String>,
    pub pool: Vec<String>,
    pub strategy: Option<Strategy>,
    pub model: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Routing {
    pub auto_switch: bool,
    pub strategy: Strategy,
    pub weekly_threshold: f64,
    pub cooldown_seconds: u64,
    pub hysteresis: f64,
    pub max_usage_age_seconds: u64,
    pub idle_release_seconds: u64,
    pub shutdown_seconds: u64,
    pub disabled: BTreeSet<String>,
    pub homes: BTreeMap<String, PathBuf>,
    pub projects: BTreeMap<PathBuf, Rule>,
    pub sessions: BTreeMap<String, Rule>,
}

impl Default for Routing {
    fn default() -> Self {
        Self {
            auto_switch: true,
            strategy: Strategy::Balanced,
            weekly_threshold: 100.0,
            cooldown_seconds: 300,
            hysteresis: 3.0,
            max_usage_age_seconds: 120,
            idle_release_seconds: 60,
            shutdown_seconds: 10,
            disabled: BTreeSet::new(),
            homes: BTreeMap::new(),
            projects: BTreeMap::new(),
            sessions: BTreeMap::new(),
        }
    }
}

impl Routing {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.weekly_threshold > 0.0 && self.weekly_threshold <= 100.0,
            "Wochengrenze muss über 0 und höchstens 100 sein"
        );
        ensure!(
            (0.0..=25.0).contains(&self.hysteresis),
            "Hysterese muss zwischen 0 und 25 liegen"
        );
        ensure!(
            (5..=300).contains(&self.max_usage_age_seconds),
            "Routing darf nur 5 bis 300 Sekunden alte Auslastungen verwenden"
        );
        ensure!(
            (5..=3600).contains(&self.idle_release_seconds),
            "Leerlauffrist muss zwischen 5 und 3600 Sekunden liegen"
        );
        ensure!(
            (1..=120).contains(&self.shutdown_seconds),
            "Abschlussfrist muss zwischen 1 und 120 Sekunden liegen"
        );
        ensure!(
            self.cooldown_seconds <= 3600,
            "Wechselpause darf höchstens 3600 Sekunden betragen"
        );
        for (path, rule) in &self.projects {
            ensure!(path.is_absolute(), "Projektpfade müssen absolut sein");
            validate_rule(rule)?;
        }
        for (id, rule) in &self.sessions {
            crate::validate_name(id)?;
            validate_rule(rule)?;
        }
        for name in &self.disabled {
            crate::validate_name(name)?;
        }
        let mut homes = BTreeSet::new();
        for (name, path) in &self.homes {
            crate::validate_name(name)?;
            ensure!(
                path.is_absolute(),
                "Native Claude-Konfigurationen brauchen absolute Pfade"
            );
            ensure!(
                homes.insert(path),
                "Jeder Account braucht ein eigenes Claude-Konfigurationsverzeichnis"
            );
        }
        Ok(())
    }

    pub fn rule(&self, cwd: &Path, session: Option<&str>) -> Rule {
        if let Some(rule) = session.and_then(|id| self.sessions.get(id)) {
            return rule.clone();
        }
        self.projects
            .iter()
            .filter(|(path, _)| cwd.starts_with(path))
            .max_by_key(|(path, _)| path.components().count())
            .map(|(_, rule)| rule.clone())
            .unwrap_or_default()
    }
}

pub fn validate_rule(rule: &Rule) -> Result<()> {
    if let Some(name) = &rule.account {
        crate::validate_name(name)?;
    }
    for name in &rule.pool {
        crate::validate_name(name)?;
    }
    ensure!(
        rule.account.is_none() || rule.pool.is_empty(),
        "Fester Account und Account-Pool schließen sich aus"
    );
    if let Some(model) = &rule.model {
        ensure!(
            !model.trim().is_empty() && model.len() <= 120,
            "Modellname ist ungültig"
        );
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Observation {
    pub name: String,
    pub usage: Option<Usage>,
    pub models: BTreeMap<String, Bucket>,
    pub fetched_at: u64,
    pub stops: Stops,
    pub busy: bool,
    pub sessions: usize,
    pub healthy: bool,
    pub problem: Option<String>,
}

impl Observation {
    fn available(&self, rule: &Rule, routing: &Routing, threshold: f64, now: u64) -> bool {
        if self.busy
            || !self.healthy
            || now.saturating_sub(self.fetched_at) > routing.max_usage_age_seconds
        {
            return false;
        }
        let Some(usage) = self.usage else {
            return false;
        };
        let five = self.stops.five_hour.unwrap_or(threshold).min(threshold);
        let seven = self
            .stops
            .seven_day
            .unwrap_or(routing.weekly_threshold)
            .min(routing.weekly_threshold);
        if usage.five_hour.utilization >= five || usage.seven_day.utilization >= seven {
            return false;
        }
        match &rule.model {
            None => true,
            Some(model) => self
                .models
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case(model))
                .is_some_and(|(_, bucket)| bucket.utilization < routing.weekly_threshold),
        }
    }

    fn load(&self) -> f64 {
        self.usage.map_or(100.0, |usage| {
            usage.five_hour.utilization.max(usage.seven_day.utilization)
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum Selection {
    Use { account: String, reason: String },
    Wait { reason: String },
}

pub struct SelectContext<'a> {
    pub current: Option<&'a str>,
    pub last_switch: u64,
    pub now: u64,
    pub threshold: f64,
}

pub fn select(
    accounts: &[Observation],
    rule: &Rule,
    routing: &Routing,
    context: SelectContext<'_>,
) -> Selection {
    let SelectContext {
        current,
        last_switch,
        now,
        threshold,
    } = context;
    let strategy = rule.strategy.unwrap_or(routing.strategy);
    let mut available: Vec<&Observation> = accounts
        .iter()
        .filter(|a| {
            !routing.disabled.contains(&a.name)
                && (rule.pool.is_empty() || rule.pool.contains(&a.name))
                && rule.account.as_ref().is_none_or(|name| name == &a.name)
                && a.available(rule, routing, threshold, now)
        })
        .collect();
    if available.is_empty() {
        return Selection::Wait {
            reason:
                "Kein freier Account mit bestätigtem Kontingent; laufende Arbeit bleibt unverändert"
                    .into(),
        };
    }
    let current_free = current.and_then(|name| available.iter().copied().find(|a| a.name == name));
    if let Some(account) = current_free {
        if rule.account.is_some()
            || !routing.auto_switch
            || strategy != Strategy::ConsumeFirst
            || now.saturating_sub(last_switch) < routing.cooldown_seconds
        {
            return Selection::Use {
                account: account.name.clone(),
                reason: "Aktueller Account ist unter seiner Grenze".into(),
            };
        }
    } else if !routing.auto_switch && rule.account.is_none() && current.is_some() {
        return Selection::Wait {
            reason: "Automatischer Session-Wechsel ist ausgeschaltet".into(),
        };
    }
    available.sort_by(|a, b| {
        let order = match strategy {
            Strategy::Best => a.load().total_cmp(&b.load()),
            Strategy::Balanced => a
                .sessions
                .cmp(&b.sessions)
                .then_with(|| a.load().total_cmp(&b.load())),
            Strategy::ConsumeFirst => {
                let reset = |a: &Observation| {
                    a.usage
                        .and_then(|u| u.seven_day.resets_at)
                        .map(|v| v.timestamp())
                        .unwrap_or(i64::MAX)
                };
                reset(a)
                    .cmp(&reset(b))
                    .then_with(|| a.load().total_cmp(&b.load()))
            }
            Strategy::NextAvailable => {
                let after = |a: &Observation| current.is_some_and(|name| a.name.as_str() <= name);
                after(a).cmp(&after(b)).then_with(|| a.name.cmp(&b.name))
            }
        };
        order.then_with(|| a.name.cmp(&b.name))
    });
    let target = available[0];
    if let Some(active) = current_free
        && active.name != target.name
    {
        let landing = target.usage.map_or(100.0, |u| u.five_hour.utilization);
        if landing + routing.hysteresis >= threshold {
            return Selection::Use {
                account: active.name.clone(),
                reason: "Wechsel wegen zu kleinem Sicherheitspuffer zurückgestellt".into(),
            };
        }
    }
    Selection::Use {
        account: target.name.clone(),
        reason: format!(
            "Auswahl nach {strategy:?}; 5h unter {threshold:.0}% und Wochenkontingent verfügbar"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn account(name: &str, five: f64, sessions: usize) -> Observation {
        Observation {
            name: name.into(),
            usage: Some(Usage {
                five_hour: Bucket {
                    utilization: five,
                    resets_at: None,
                },
                seven_day: Bucket {
                    utilization: 10.0,
                    resets_at: None,
                },
            }),
            models: BTreeMap::new(),
            fetched_at: 1000,
            stops: Stops::default(),
            busy: false,
            sessions,
            healthy: true,
            problem: None,
        }
    }
    fn choice(
        accounts: &[Observation],
        current: Option<&str>,
        rule: &Rule,
        routing: &Routing,
    ) -> Selection {
        select(
            accounts,
            rule,
            routing,
            SelectContext {
                current,
                last_switch: 999,
                now: 1000,
                threshold: 98.0,
            },
        )
    }
    #[test]
    fn switches_at_98_even_inside_cooldown() {
        let result = choice(
            &[account("a", 98.0, 1), account("b", 12.0, 0)],
            Some("a"),
            &Rule::default(),
            &Routing::default(),
        );
        assert!(matches!(result, Selection::Use { account, .. } if account == "b"));
    }
    #[test]
    fn stays_at_97_and_spreads_new_sessions() {
        let accounts = [account("a", 97.0, 1), account("b", 12.0, 0)];
        assert!(
            matches!(choice(&accounts,Some("a"), &Rule::default(), &Routing::default()),Selection::Use { account, .. } if account == "a")
        );
        assert!(
            matches!(choice(&accounts,None, &Rule::default(), &Routing::default()),Selection::Use { account, .. } if account == "b")
        );
    }
    #[test]
    fn unknown_stale_disabled_and_busy_never_become_targets() {
        let mut accounts = vec![
            account("a", 0.0, 0),
            account("b", 0.0, 0),
            account("c", 0.0, 0),
            account("d", 0.0, 0),
        ];
        accounts[0].usage = None;
        accounts[1].fetched_at = 0;
        accounts[2].busy = true;
        let mut routing = Routing::default();
        routing.disabled.insert("d".into());
        assert!(matches!(
            choice(&accounts, None, &Rule::default(), &routing),
            Selection::Wait { .. }
        ));
    }
    #[test]
    fn fixed_accounts_and_model_limits_fail_closed() {
        let accounts = [account("a", 100.0, 0), account("b", 0.0, 0)];
        let rule = Rule {
            account: Some("a".into()),
            ..Rule::default()
        };
        assert!(matches!(
            choice(&accounts, None, &rule, &Routing::default()),
            Selection::Wait { .. }
        ));
        let rule = Rule {
            model: Some("custom-model".into()),
            ..Rule::default()
        };
        assert!(matches!(
            choice(&accounts, None, &rule, &Routing::default()),
            Selection::Wait { .. }
        ));
    }
    #[test]
    fn closest_parent_wins_without_prefix_collision() {
        let mut routing = Routing::default();
        routing.projects.insert(
            PathBuf::from("/work"),
            Rule {
                account: Some("a".into()),
                ..Rule::default()
            },
        );
        routing.projects.insert(
            PathBuf::from("/work/app"),
            Rule {
                account: Some("b".into()),
                ..Rule::default()
            },
        );
        assert_eq!(
            routing
                .rule(Path::new("/work/app/src"), None)
                .account
                .as_deref(),
            Some("b")
        );
        assert_eq!(routing.rule(Path::new("/workspace"), None).account, None);
    }
}
