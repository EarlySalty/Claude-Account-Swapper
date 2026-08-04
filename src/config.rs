//! Dauerhafte Einstellungen des Switchers.
//!
//! Die Datei ist die einzige Quelle fuer alles, was der Hintergrunddienst tun *darf*. Sie liegt
//! bewusst nicht in der systemd-Unit: der Dienst liest sie bei jeder Pruefung neu, damit eine
//! Aenderung im Menue sofort wirkt und niemand `systemctl` bedienen muss.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::usage::DEFAULT_SWITCH_THRESHOLD;

pub const DEFAULT_PING_PROMPT: &str = "Bist du da?";
/// Der Ping soll das Fenster eroeffnen, nicht Kontingent verbrauchen; das kleinste Modell reicht.
pub const DEFAULT_PING_MODEL: &str = "haiku";
pub const DEFAULT_PING_TIMEOUT_MINUTES: u64 = 5;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Bei vollem Limit selbst auf einen freien Account wechseln.
    pub auto_switch: bool,
    /// Ab dieser Auslastung in Prozent gilt ein Fenster als verbraucht.
    pub threshold: f64,
    pub ping: Ping,
}

/// Startet das Fuenf-Stunden-Fenster von selbst neu, sobald es zurueckgesetzt wurde.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Ping {
    pub enabled: bool,
    pub prompt: String,
    pub model: String,
    pub timeout_minutes: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            auto_switch: false,
            threshold: DEFAULT_SWITCH_THRESHOLD,
            ping: Ping::default(),
        }
    }
}

impl Default for Ping {
    fn default() -> Self {
        Self {
            enabled: false,
            prompt: DEFAULT_PING_PROMPT.to_owned(),
            model: DEFAULT_PING_MODEL.to_owned(),
            timeout_minutes: DEFAULT_PING_TIMEOUT_MINUTES,
        }
    }
}

impl Config {
    pub fn parse(text: &str) -> Result<Self> {
        let config: Self =
            serde_json::from_str(text).context("Einstellungen sind kein gueltiges JSON")?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            (0.0..=100.0).contains(&self.threshold),
            "Schwelle muss zwischen 0 und 100 liegen"
        );
        anyhow::ensure!(
            !self.ping.prompt.trim().is_empty(),
            "Ping-Text darf nicht leer sein"
        );
        anyhow::ensure!(
            self.ping.timeout_minutes > 0,
            "Zeitlimit des Pings muss groesser als 0 sein"
        );
        Ok(())
    }

    /// Ein Zustandssatz fuers Log. Der Dienst schreibt ihn nur, wenn er sich geaendert hat -
    /// sonst stuende dieselbe Zeile jede Minute im Journal.
    pub fn describe(&self) -> String {
        format!(
            "Auto-Wechsel {} (Schwelle {:.0}%), Fenster-Ping {}",
            if self.auto_switch { "an" } else { "aus" },
            self.threshold,
            if self.ping.enabled { "an" } else { "aus" }
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fehlende_felder_fallen_auf_die_standardwerte_zurueck() {
        let config = Config::parse(r#"{"ping":{"enabled":true}}"#).expect("Einstellungen");
        assert!(config.ping.enabled);
        assert_eq!(config.ping.prompt, DEFAULT_PING_PROMPT);
        assert_eq!(config.ping.model, DEFAULT_PING_MODEL);
        assert!(!config.auto_switch);
        assert_eq!(config.threshold, DEFAULT_SWITCH_THRESHOLD);
    }

    #[test]
    fn eine_leere_datei_ist_die_standardkonfiguration() {
        assert_eq!(
            Config::parse("{}").expect("Einstellungen"),
            Config::default()
        );
    }

    /// Eine unsinnige Schwelle wuerde jeden Wechsel entweder verhindern oder erzwingen; sie
    /// darf nicht stillschweigend uebernommen werden.
    #[test]
    fn unsinnige_werte_sind_ein_fehler() {
        assert!(Config::parse(r#"{"threshold":150}"#).is_err());
        assert!(Config::parse(r#"{"ping":{"prompt":"  "}}"#).is_err());
        assert!(Config::parse(r#"{"ping":{"timeout_minutes":0}}"#).is_err());
        assert!(Config::parse("kein json").is_err());
    }

    #[test]
    fn der_zustandssatz_nennt_beide_schalter() {
        let mut config = Config::default();
        assert_eq!(
            config.describe(),
            "Auto-Wechsel aus (Schwelle 98%), Fenster-Ping aus"
        );
        config.auto_switch = true;
        config.ping.enabled = true;
        assert_eq!(
            config.describe(),
            "Auto-Wechsel an (Schwelle 98%), Fenster-Ping an"
        );
    }
}
