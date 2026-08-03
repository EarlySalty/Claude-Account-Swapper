//! Nutzungslimits eines Claude-Accounts abfragen und daraus einen Wechsel ableiten.
//!
//! Anthropic liefert die Auslastung zum OAuth-Login, nicht zum Geraet. Damit laesst sich die
//! Auslastung eines Accounts auch dann lesen, wenn er gerade nicht aktiv ist - genau das macht
//! einen vorausschauenden Wechsel moeglich. Die Abfrage verbraucht selbst kein Kontingent.

use std::cmp::Ordering;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Standardschwelle, ab der ein Fuenf-Stunden-Fenster als verbraucht gilt.
pub const DEFAULT_SWITCH_THRESHOLD: f64 = 98.0;
/// Ein haengender Request wuerde sonst den Hintergrunddienst anhalten.
const REQUEST_TIMEOUT_SECONDS: u64 = 10;
const DEFAULT_USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";

/// Ein Limitfenster: wie voll es ist und wann es sich zuruecksetzt.
#[derive(Debug, Clone, Copy, Deserialize, PartialEq)]
pub struct Bucket {
    /// Auslastung in Prozent.
    pub utilization: f64,
    /// Fehlt, solange in diesem Fenster nichts verbraucht wurde: dann laeuft gar keine Frist,
    /// die zuruecksetzen koennte. Die API liefert dafuer `null`.
    pub resets_at: Option<DateTime<Utc>>,
}

/// Die beiden Fenster, die ueber die Nutzbarkeit eines Accounts entscheiden.
#[derive(Debug, Clone, Copy, Deserialize, PartialEq)]
pub struct Usage {
    pub five_hour: Bucket,
    pub seven_day: Bucket,
}

/// Wie weit ein einzelner Account benutzt werden darf.
///
/// Ohne Eintrag gilt die globale Schwelle. Wer einem Account eine Reserve lassen will - etwa
/// weil das Wochenkontingent fuer einen bestimmten Zweck freibleiben soll - setzt hier einen
/// niedrigeren Wert; der Account wird dann schon vorher verlassen.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq)]
pub struct Stops {
    /// Grenze fuer das Fuenf-Stunden-Fenster in Prozent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub five_hour: Option<f64>,
    /// Grenze fuer das Wochenfenster in Prozent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seven_day: Option<f64>,
    /// Harte Grenze: dieser Account wird auch dann nicht angebrochen, wenn sonst gar kein
    /// Account mehr frei ist. Ohne das Kennzeichen ist die Grenze eine Reserve, die im
    /// Notfall genutzt wird - protokolliert, damit es nicht unbemerkt passiert.
    #[serde(default)]
    pub hard: bool,
}

impl Stops {
    fn five_hour_or(&self, fallback: f64) -> f64 {
        self.five_hour.unwrap_or(fallback)
    }

    fn seven_day_or(&self, fallback: f64) -> f64 {
        self.seven_day.unwrap_or(fallback)
    }

    pub fn is_set(&self) -> bool {
        self.five_hour.is_some() || self.seven_day.is_some()
    }
}

/// Ein Account, dessen Auslastung gelesen werden konnte, samt seiner eigenen Grenzen.
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    pub name: String,
    pub usage: Usage,
    pub stops: Stops,
}

impl Candidate {
    /// Der Account ist unter seinen *eigenen* Grenzen - er darf regulaer benutzt werden.
    fn within_stops(&self, threshold: f64) -> bool {
        self.usage.is_free_with(
            self.stops.five_hour_or(threshold),
            self.stops.seven_day_or(threshold),
        )
    }

    /// Der Account hat noch echtes Kontingent, seine eigene Reserve waere aber angebrochen.
    /// Bei einer harten Grenze gibt es diesen Fall nicht.
    fn within_global_limit(&self, threshold: f64) -> bool {
        !self.stops.hard && self.usage.is_free_with(threshold, threshold)
    }

    /// Wann dieser Account wieder benutzbar wird. Eine harte Grenze verschiebt den Zeitpunkt,
    /// eine weiche nicht: die duerfte man ja notfalls anbrechen.
    fn unblocked_at(&self, threshold: f64) -> Option<DateTime<Utc>> {
        if self.stops.hard {
            self.usage.unblocked_at_with(
                self.stops.five_hour_or(threshold),
                self.stops.seven_day_or(threshold),
            )
        } else {
            self.usage.unblocked_at_with(threshold, threshold)
        }
    }

    fn describe(&self, threshold: f64) -> String {
        let five_stop = self.stops.five_hour_or(threshold);
        let seven_stop = self.stops.seven_day_or(threshold);
        let mut text = format!(
            "5h {:.0}%/{:.0}%, 7d {:.0}%/{:.0}%",
            self.usage.five_hour.utilization,
            five_stop,
            self.usage.seven_day.utilization,
            seven_stop
        );
        if self.stops.is_set() {
            text.push_str(if self.stops.hard {
                ", eigene Grenze (hart)"
            } else {
                ", eigene Grenze"
            });
        }
        text
    }
}

impl Usage {
    /// Beide Fenster liegen unter der Schwelle - der Account ist benutzbar.
    pub fn is_free(&self, threshold: f64) -> bool {
        self.is_free_with(threshold, threshold)
    }

    fn is_free_with(&self, five_hour: f64, seven_day: f64) -> bool {
        self.five_hour.utilization < five_hour && self.seven_day.utilization < seven_day
    }

    /// Wann dieser Account frei wird: der spaeteste Reset unter den Fenstern, die ihn gerade
    /// blockieren. Ein Account ist erst dann wieder nutzbar, wenn *alle* vollen Fenster
    /// zurueckgesetzt sind; der frueheste Reset waere hier die falsche Auskunft.
    ///
    /// `None` heisst "nicht datierbar" - entweder blockiert nichts, oder ein blockierendes
    /// Fenster nennt keinen Zeitpunkt. Beides schliesst den Account als Wartekandidaten aus:
    /// ein unbekannter Zeitpunkt darf nie als der fruehere durchgehen.
    fn unblocked_at_with(&self, five_hour: f64, seven_day: f64) -> Option<DateTime<Utc>> {
        let mut blocked: Option<DateTime<Utc>> = None;
        for (bucket, threshold) in [(self.five_hour, five_hour), (self.seven_day, seven_day)] {
            if bucket.utilization >= threshold {
                let resets_at = bucket.resets_at?;
                blocked = Some(match blocked {
                    Some(current) => current.max(resets_at),
                    None => resets_at,
                });
            }
        }
        blocked
    }
}

/// Was mit dem aktiven Account geschehen soll. Jede Variante traegt ihre Begruendung mit:
/// eine Entscheidung ohne nachvollziehbaren Grund ist im Log wertlos.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Stay { reason: String },
    SwitchTo { name: String, reason: String },
    NoCandidate { reason: String },
}

impl Decision {
    pub fn reason(&self) -> &str {
        match self {
            Self::Stay { reason }
            | Self::SwitchTo { reason, .. }
            | Self::NoCandidate { reason } => reason,
        }
    }
}

/// Waehlt den Account, auf dem weitergearbeitet werden soll.
///
/// Rein und ohne Seiteneffekte: `accounts` enthaelt jeden Account, dessen Auslastung wirklich
/// gelesen werden konnte - unerreichbare Accounts fehlen hier bewusst, damit sie nie gewaehlt
/// werden. Fehlt darin der aktive Account, wird nicht geraten, sondern gar nicht gewechselt.
/// Die Auswahl laeuft in drei Stufen, weil eine enge eigene Grenze sonst jeden Wechsel
/// verhindern wuerde: erst Accounts unter ihrer eigenen Grenze, dann - falls es keinen gibt -
/// Accounts mit echtem Restkontingent, deren Reserve dafuer angebrochen wird, und zuletzt der
/// Account, der am fruehesten wieder frei ist.
pub fn pick_target(active: &str, accounts: &[Candidate], threshold: f64) -> Decision {
    let Some(current) = accounts.iter().find(|candidate| candidate.name == active) else {
        return Decision::NoCandidate {
            reason: format!(
                "Auslastung von `{active}` ist unbekannt; ohne sie wird nicht gewechselt"
            ),
        };
    };

    if current.within_stops(threshold) {
        return Decision::Stay {
            reason: format!(
                "`{active}` ist unter seiner Grenze ({})",
                current.describe(threshold)
            ),
        };
    }

    if let Some(target) = best(
        accounts
            .iter()
            .filter(|candidate| candidate.name != active && candidate.within_stops(threshold)),
    ) {
        return Decision::SwitchTo {
            name: target.name.clone(),
            reason: format!(
                "`{active}` hat seine Grenze erreicht ({}); `{}` ist frei ({})",
                current.describe(threshold),
                target.name,
                target.describe(threshold)
            ),
        };
    }

    // Kein Account unter seiner eigenen Grenze. Wer noch echtes Kontingent hat, kommt jetzt
    // trotzdem in Frage - seine Reserve wird angebrochen, und genau das steht auch so im Log.
    // Der aktive Account ist hier bewusst dabei: bringt ein Wechsel nichts, bleibt er stehen.
    if let Some(target) = best(
        accounts
            .iter()
            .filter(|candidate| candidate.within_global_limit(threshold)),
    ) {
        if target.name == active {
            return Decision::Stay {
                reason: format!(
                    "kein Account ist unter seiner Grenze; `{active}` ({}) hat noch am meisten \
                     Kontingent, seine Reserve wird angebrochen",
                    current.describe(threshold)
                ),
            };
        }
        return Decision::SwitchTo {
            name: target.name.clone(),
            reason: format!(
                "kein Account ist unter seiner Grenze; `{active}` ({}) wird verlassen, \
                 die Reserve von `{}` ({}) wird angebrochen",
                current.describe(threshold),
                target.name,
                target.describe(threshold)
            ),
        };
    }

    // Auch kein Restkontingent. Dann gewinnt der, der am fruehesten wieder frei wird - warten
    // muss man ohnehin, die Frage ist nur, wo am kuerzesten.
    let mut soonest: Vec<(&Candidate, DateTime<Utc>)> = accounts
        .iter()
        .filter_map(|candidate| {
            candidate
                .unblocked_at(threshold)
                .map(|unblocked| (candidate, unblocked))
        })
        .collect();
    soonest.sort_by(|(left, left_at), (right, right_at)| {
        left_at
            .cmp(right_at)
            .then_with(|| left.name.cmp(&right.name))
    });
    let Some((target, unblocked)) = soonest.first() else {
        return Decision::NoCandidate {
            reason: format!("`{active}` ist voll, aber kein Account meldet ein Reset-Fenster"),
        };
    };

    if target.name == active {
        return Decision::Stay {
            reason: format!(
                "alle Accounts sind voll; `{active}` ({}) wird als erster wieder frei",
                current.describe(threshold)
            ),
        };
    }
    Decision::SwitchTo {
        name: target.name.clone(),
        reason: format!(
            "alle Accounts sind voll; `{active}` ({}) wartet laenger als `{}` ({}, frei ab {})",
            current.describe(threshold),
            target.name,
            target.describe(threshold),
            unblocked.with_timezone(&chrono::Local).format("%F %H:%M")
        ),
    }
}

/// Der freieste Account einer Auswahl - deterministisch auch bei Gleichstand.
fn best<'a>(candidates: impl Iterator<Item = &'a Candidate>) -> Option<&'a Candidate> {
    candidates.min_by(|left, right| {
        compare_utilization(&left.usage, &right.usage).then_with(|| left.name.cmp(&right.name))
    })
}

/// Weniger verbrauchtes Fuenf-Stunden-Fenster gewinnt; bei Gleichstand das freiere Wochenfenster.
/// `total_cmp` statt `partial_cmp`, damit die Sortierung auch bei einem NaN aus der API haelt.
fn compare_utilization(left: &Usage, right: &Usage) -> Ordering {
    left.five_hour
        .utilization
        .total_cmp(&right.five_hour.utilization)
        .then_with(|| {
            left.seven_day
                .utilization
                .total_cmp(&right.seven_day.utilization)
        })
}

/// Fragt die Auslastung zu einem Access-Token ab.
///
/// Der Token wandert ausschliesslich in den Authorization-Header: als Kommandozeilenargument
/// stuende er in der Prozessliste, und in einer Fehlermeldung stuende er dauerhaft im Journal.
/// Deshalb enthaelt auch kein Fehlertext dieser Funktion den Token.
pub fn fetch(access_token: &str) -> Result<Usage> {
    let url = std::env::var("CLAUDE_ACCOUNT_SWITCHER_USAGE_URL")
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| DEFAULT_USAGE_URL.to_owned());

    let agent = ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(REQUEST_TIMEOUT_SECONDS)))
            // Ein 401 ist eine Aussage ueber den Login, kein Transportfehler; er soll als
            // Statuscode ankommen und nicht als anonymer Request-Fehler.
            .http_status_as_error(false)
            .build(),
    );
    let mut response = agent
        .get(&url)
        .header("Authorization", &format!("Bearer {access_token}"))
        .header("Accept", "application/json")
        .call()
        .map_err(|error| anyhow::anyhow!("Auslastung konnte nicht abgefragt werden: {error}"))?;

    let status = response.status().as_u16();
    let body = response
        .body_mut()
        .read_to_string()
        .context("Antwort der Auslastungs-API konnte nicht gelesen werden")?;
    if status != 200 {
        bail!("Auslastungs-API antwortete mit HTTP {status}");
    }
    parse(&body)
}

/// Liest die beiden interessanten Fenster aus der Antwort.
///
/// Die API liefert etliche weitere Felder, die kommen und gehen; deshalb wird gezielt gelesen
/// statt die ganze Antwort in eine Struktur zu zwingen, die beim naechsten Zusatzfeld bricht.
pub fn parse(body: &str) -> Result<Usage> {
    let value: Value =
        serde_json::from_str(body).context("Auslastungs-API lieferte kein gueltiges JSON")?;
    Ok(Usage {
        five_hour: bucket(&value, "five_hour")?,
        seven_day: bucket(&value, "seven_day")?,
    })
}

fn bucket(value: &Value, field: &str) -> Result<Bucket> {
    let bucket = value
        .get(field)
        .with_context(|| format!("Auslastungs-API meldet kein Feld `{field}`"))?;
    let utilization = bucket
        .get("utilization")
        .and_then(Value::as_f64)
        .with_context(|| format!("`{field}` enthaelt keine Auslastung"))?;
    // `resets_at` fehlt bei einem ungenutzten Fenster; nur ein *unlesbarer* Wert ist ein Fehler.
    let resets_at = match bucket.get("resets_at").and_then(Value::as_str) {
        Some(text) => Some(
            DateTime::parse_from_rfc3339(text)
                .with_context(|| format!("Reset-Zeitpunkt von `{field}` ist unlesbar"))?
                .with_timezone(&Utc),
        ),
        None => None,
    };
    Ok(Bucket {
        utilization,
        resets_at,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(text: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(text)
            .expect("Zeitstempel")
            .with_timezone(&Utc)
    }

    fn usage(five: f64, seven: f64) -> Usage {
        usage_at(five, seven, "2026-08-03T20:00:00Z", "2026-08-09T02:00:00Z")
    }

    fn usage_at(five: f64, seven: f64, five_reset: &str, seven_reset: &str) -> Usage {
        Usage {
            five_hour: Bucket {
                utilization: five,
                resets_at: Some(at(five_reset)),
            },
            seven_day: Bucket {
                utilization: seven,
                resets_at: Some(at(seven_reset)),
            },
        }
    }

    fn accounts(entries: &[(&str, Usage)]) -> Vec<Candidate> {
        entries
            .iter()
            .map(|(name, usage)| Candidate {
                name: (*name).to_owned(),
                usage: *usage,
                stops: Stops::default(),
            })
            .collect()
    }

    fn stops(five: Option<f64>, seven: Option<f64>, hard: bool) -> Stops {
        Stops {
            five_hour: five,
            seven_day: seven,
            hard,
        }
    }

    fn accounts_with_stops(entries: &[(&str, Usage, Stops)]) -> Vec<Candidate> {
        entries
            .iter()
            .map(|(name, usage, stops)| Candidate {
                name: (*name).to_owned(),
                usage: *usage,
                stops: *stops,
            })
            .collect()
    }

    #[test]
    fn aktiver_account_unter_der_schwelle_bleibt() {
        let accounts = accounts(&[("a", usage(19.0, 33.0)), ("b", usage(0.0, 0.0))]);
        let decision = pick_target("a", &accounts, 98.0);
        assert!(matches!(decision, Decision::Stay { .. }), "{decision:?}");
        assert!(decision.reason().contains("5h 19%"), "{decision:?}");
    }

    #[test]
    fn schwelle_ist_erreicht_nicht_erst_ueberschritten() {
        let accounts = accounts(&[("a", usage(98.0, 10.0)), ("b", usage(5.0, 5.0))]);
        assert_eq!(
            pick_target("a", &accounts, 98.0),
            Decision::SwitchTo {
                name: "b".to_owned(),
                reason: "`a` hat seine Grenze erreicht (5h 98%/98%, 7d 10%/98%); \
                         `b` ist frei (5h 5%/98%, 7d 5%/98%)"
                    .to_owned(),
            }
        );
    }

    #[test]
    fn knapp_unter_der_schwelle_bleibt_aktiv() {
        let accounts = accounts(&[("a", usage(97.9, 10.0)), ("b", usage(0.0, 0.0))]);
        assert!(matches!(
            pick_target("a", &accounts, 98.0),
            Decision::Stay { .. }
        ));
    }

    #[test]
    fn freiester_account_gewinnt() {
        let accounts = accounts(&[
            ("a", usage(100.0, 50.0)),
            ("b", usage(40.0, 10.0)),
            ("c", usage(5.0, 90.0)),
        ]);
        match pick_target("a", &accounts, 98.0) {
            Decision::SwitchTo { name, .. } => assert_eq!(name, "c"),
            other => panic!("unerwartet: {other:?}"),
        }
    }

    #[test]
    fn bei_gleichem_fuenfstundenfenster_entscheidet_die_woche() {
        let accounts = accounts(&[
            ("a", usage(100.0, 50.0)),
            ("b", usage(20.0, 80.0)),
            ("c", usage(20.0, 30.0)),
        ]);
        match pick_target("a", &accounts, 98.0) {
            Decision::SwitchTo { name, .. } => assert_eq!(name, "c"),
            other => panic!("unerwartet: {other:?}"),
        }
    }

    #[test]
    fn gleichstand_entscheidet_der_name_und_bleibt_stabil() {
        let accounts = accounts(&[
            ("aktiv", usage(100.0, 50.0)),
            ("zeta", usage(20.0, 30.0)),
            ("alpha", usage(20.0, 30.0)),
        ]);
        match pick_target("aktiv", &accounts, 98.0) {
            Decision::SwitchTo { name, .. } => assert_eq!(name, "alpha"),
            other => panic!("unerwartet: {other:?}"),
        }
    }

    #[test]
    fn volles_wochenfenster_schliesst_einen_sonst_freien_account_aus() {
        let accounts = accounts(&[
            ("a", usage(100.0, 10.0)),
            ("b", usage(1.0, 99.0)),
            ("c", usage(70.0, 70.0)),
        ]);
        match pick_target("a", &accounts, 98.0) {
            Decision::SwitchTo { name, .. } => assert_eq!(name, "c"),
            other => panic!("unerwartet: {other:?}"),
        }
    }

    #[test]
    fn ohne_freien_account_gewinnt_der_fruehere_reset() {
        let accounts = accounts(&[
            (
                "a",
                usage_at(100.0, 10.0, "2026-08-03T23:00:00Z", "2026-08-09T02:00:00Z"),
            ),
            (
                "b",
                usage_at(100.0, 10.0, "2026-08-03T20:00:00Z", "2026-08-09T02:00:00Z"),
            ),
        ]);
        match pick_target("a", &accounts, 98.0) {
            Decision::SwitchTo { name, .. } => assert_eq!(name, "b"),
            other => panic!("unerwartet: {other:?}"),
        }
    }

    #[test]
    fn ein_volles_wochenfenster_zaehlt_als_wartezeit_nicht_das_fuenfstundenfenster() {
        // `b` waere in einer Stunde wieder frei, wenn nur das kurze Fenster zaehlte - sein
        // Wochenlimit haelt ihn aber noch tagelang blockiert. `a` ist damit schneller wieder da.
        let accounts = accounts(&[
            (
                "a",
                usage_at(100.0, 10.0, "2026-08-03T22:00:00Z", "2026-08-09T02:00:00Z"),
            ),
            (
                "b",
                usage_at(100.0, 99.0, "2026-08-03T20:00:00Z", "2026-08-09T02:00:00Z"),
            ),
        ]);
        assert!(matches!(
            pick_target("a", &accounts, 98.0),
            Decision::Stay { .. }
        ));
    }

    #[test]
    fn aktiver_account_mit_fruehestem_reset_bleibt() {
        let accounts = accounts(&[
            (
                "a",
                usage_at(100.0, 10.0, "2026-08-03T19:00:00Z", "2026-08-09T02:00:00Z"),
            ),
            (
                "b",
                usage_at(100.0, 10.0, "2026-08-03T21:00:00Z", "2026-08-09T02:00:00Z"),
            ),
        ]);
        let decision = pick_target("a", &accounts, 98.0);
        assert!(matches!(decision, Decision::Stay { .. }), "{decision:?}");
        assert!(decision.reason().contains("als erster wieder frei"));
    }

    #[test]
    fn ohne_kandidaten_wird_nicht_gewechselt() {
        let accounts = accounts(&[("a", usage(100.0, 100.0))]);
        assert!(matches!(
            pick_target("a", &accounts, 98.0),
            Decision::Stay { .. }
        ));
        assert!(matches!(
            pick_target("a", &[], 98.0),
            Decision::NoCandidate { .. }
        ));
    }

    #[test]
    fn unbekannte_auslastung_des_aktiven_accounts_verhindert_den_wechsel() {
        let accounts = accounts(&[("b", usage(0.0, 0.0))]);
        let decision = pick_target("a", &accounts, 98.0);
        assert!(
            matches!(decision, Decision::NoCandidate { .. }),
            "{decision:?}"
        );
        assert!(decision.reason().contains("unbekannt"));
    }

    #[test]
    fn echte_antwort_wird_gelesen_und_zusatzfelder_stoeren_nicht() {
        let body = r#"{"five_hour":{"utilization":19.0,"resets_at":"2026-08-03T21:19:59.205733+00:00","limit_dollars":null},
                       "seven_day":{"utilization":33.0,"resets_at":"2026-08-09T02:59:59.205758+00:00","used_dollars":null},
                       "seven_day_opus":null,"iguana_necktie":null,
                       "limits":[{"kind":"session","percent":19,"severity":"normal","resets_at":"2026-08-03T21:19:59.205733+00:00","is_active":false}],
                       "extra_usage":{"is_enabled":false}}"#;
        let usage = parse(body).expect("Antwort");
        assert_eq!(usage.five_hour.utilization, 19.0);
        assert_eq!(usage.seven_day.utilization, 33.0);
        assert_eq!(
            usage.five_hour.resets_at,
            Some(at("2026-08-03T21:19:59.205733Z"))
        );
    }

    /// Echte Antwort eines Accounts, in dessen Fuenf-Stunden-Fenster nichts verbraucht wurde:
    /// `resets_at` ist dann `null`. Das ist kein Fehler, sondern der Normalfall bei 0 %.
    #[test]
    fn ein_ungenutztes_fenster_ohne_reset_zeitpunkt_ist_kein_fehler() {
        let body = r#"{"five_hour":{"utilization":0.0,"resets_at":null},
                       "seven_day":{"utilization":100.0,"resets_at":"2026-08-04T19:59:59.038985+00:00"}}"#;
        let usage = parse(body).expect("Antwort");
        assert_eq!(usage.five_hour.resets_at, None);
        assert!(!usage.is_free(98.0), "das Wochenlimit ist voll");
    }

    /// Ein blockierender Account ohne Reset-Zeitpunkt darf nicht als der schnellste gelten:
    /// unbekannt ist nicht dasselbe wie sofort.
    #[test]
    fn ohne_datierbaren_reset_gewinnt_ein_account_das_warten_nicht() {
        let unklar = Usage {
            five_hour: Bucket {
                utilization: 100.0,
                resets_at: None,
            },
            seven_day: Bucket {
                utilization: 10.0,
                resets_at: None,
            },
        };
        let accounts = accounts(&[
            (
                "aktiv",
                usage_at(100.0, 10.0, "2026-08-03T23:00:00Z", "2026-08-09T02:00:00Z"),
            ),
            ("unklar", unklar),
        ]);
        let decision = pick_target("aktiv", &accounts, 98.0);
        assert!(matches!(decision, Decision::Stay { .. }), "{decision:?}");
    }

    #[test]
    fn eine_eigene_grenze_verlaesst_den_account_frueher() {
        let accounts = accounts_with_stops(&[
            ("a", usage(85.0, 20.0), stops(Some(80.0), None, false)),
            ("b", usage(50.0, 20.0), Stops::default()),
        ]);
        let decision = pick_target("a", &accounts, 98.0);
        match &decision {
            Decision::SwitchTo { name, reason } => {
                assert_eq!(name, "b");
                assert!(reason.contains("5h 85%/80%"), "{reason}");
            }
            other => panic!("unerwartet: {other:?}"),
        }
    }

    #[test]
    fn eine_eigene_wochengrenze_wirkt_genauso() {
        let accounts = accounts_with_stops(&[
            ("a", usage(10.0, 55.0), stops(None, Some(50.0), false)),
            ("b", usage(60.0, 60.0), Stops::default()),
        ]);
        match pick_target("a", &accounts, 98.0) {
            Decision::SwitchTo { name, .. } => assert_eq!(name, "b"),
            other => panic!("unerwartet: {other:?}"),
        }
    }

    #[test]
    fn ein_account_unter_seiner_grenze_bleibt_stehen() {
        let accounts = accounts_with_stops(&[
            ("a", usage(79.0, 20.0), stops(Some(80.0), None, false)),
            ("b", usage(0.0, 0.0), Stops::default()),
        ]);
        assert!(matches!(
            pick_target("a", &accounts, 98.0),
            Decision::Stay { .. }
        ));
    }

    /// Eine eigene Grenze schuetzt einen Account auch davor, als *Ziel* gewaehlt zu werden -
    /// sonst waere die Reserve wertlos.
    #[test]
    fn ein_account_ueber_seiner_grenze_wird_nicht_als_ziel_gewaehlt() {
        let accounts = accounts_with_stops(&[
            ("a", usage(100.0, 20.0), Stops::default()),
            (
                "geschont",
                usage(60.0, 20.0),
                stops(Some(50.0), None, false),
            ),
            ("normal", usage(70.0, 20.0), Stops::default()),
        ]);
        match pick_target("a", &accounts, 98.0) {
            Decision::SwitchTo { name, .. } => assert_eq!(name, "normal"),
            other => panic!("unerwartet: {other:?}"),
        }
    }

    /// Wenn kein Account mehr unter seiner Grenze ist, darf eine weiche Reserve angebrochen
    /// werden - sonst wuerde eine enge Konfiguration jede Arbeit blockieren, obwohl noch
    /// Kontingent da ist. Der Vorgang wird als solcher benannt.
    #[test]
    fn ohne_freien_account_wird_eine_weiche_reserve_angebrochen() {
        let accounts = accounts_with_stops(&[
            ("a", usage(100.0, 20.0), Stops::default()),
            ("reserve", usage(60.0, 20.0), stops(Some(50.0), None, false)),
        ]);
        match pick_target("a", &accounts, 98.0) {
            Decision::SwitchTo { name, reason } => {
                assert_eq!(name, "reserve");
                assert!(reason.contains("Reserve"), "{reason}");
            }
            other => panic!("unerwartet: {other:?}"),
        }
    }

    /// Eine harte Grenze wird auch dann nicht angebrochen, wenn sonst nichts mehr frei ist -
    /// im selben Aufbau wuerde eine weiche sofort genutzt. Der Account wartet also lieber.
    #[test]
    fn eine_harte_grenze_wird_nie_angebrochen_eine_weiche_schon() {
        let aktiv = usage_at(100.0, 20.0, "2026-08-03T20:00:00Z", "2026-08-09T02:00:00Z");
        let ziel = usage_at(60.0, 20.0, "2026-08-03T23:00:00Z", "2026-08-09T02:00:00Z");

        let hart = accounts_with_stops(&[
            ("a", aktiv, Stops::default()),
            ("ziel", ziel, stops(Some(50.0), None, true)),
        ]);
        let decision = pick_target("a", &hart, 98.0);
        assert!(matches!(decision, Decision::Stay { .. }), "{decision:?}");

        let weich = accounts_with_stops(&[
            ("a", aktiv, Stops::default()),
            ("ziel", ziel, stops(Some(50.0), None, false)),
        ]);
        match pick_target("a", &weich, 98.0) {
            Decision::SwitchTo { name, reason } => {
                assert_eq!(name, "ziel");
                assert!(reason.contains("Reserve"), "{reason}");
            }
            other => panic!("unerwartet: {other:?}"),
        }
    }

    /// Der aktive Account behaelt seine eigene Reserve, solange kein anderer besser dasteht.
    #[test]
    fn ohne_besseres_ziel_bleibt_der_aktive_account_in_seiner_reserve() {
        let accounts = accounts_with_stops(&[
            ("a", usage(60.0, 20.0), stops(Some(50.0), None, false)),
            ("b", usage(90.0, 20.0), stops(Some(50.0), None, false)),
        ]);
        let decision = pick_target("a", &accounts, 98.0);
        assert!(matches!(decision, Decision::Stay { .. }), "{decision:?}");
        assert!(decision.reason().contains("Reserve"), "{decision:?}");
    }

    /// Eine harte Grenze verschiebt auch die Wartezeit: der Account ist erst wieder nutzbar,
    /// wenn sein eigenes Fenster zurueckgesetzt ist, nicht schon bei 98 Prozent.
    #[test]
    fn eine_harte_grenze_zaehlt_auch_fuer_die_wartezeit() {
        let accounts = accounts_with_stops(&[
            (
                "a",
                usage_at(100.0, 20.0, "2026-08-03T23:00:00Z", "2026-08-09T02:00:00Z"),
                Stops::default(),
            ),
            (
                "tabu",
                usage_at(60.0, 20.0, "2026-08-03T20:00:00Z", "2026-08-09T02:00:00Z"),
                stops(Some(50.0), None, true),
            ),
        ]);
        match pick_target("a", &accounts, 98.0) {
            Decision::SwitchTo { name, .. } => assert_eq!(name, "tabu"),
            other => panic!("unerwartet: {other:?}"),
        }
    }

    #[test]
    fn fehlende_felder_sind_ein_fehler_und_kein_stiller_nullwert() {
        assert!(
            parse(r#"{"seven_day":{"utilization":1.0,"resets_at":"2026-08-03T21:00:00Z"}}"#)
                .is_err()
        );
        assert!(parse("kein json").is_err());
    }
}
