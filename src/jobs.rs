//! Aufgaben, die auf ein frisches Fuenf-Stunden-Fenster warten.
//!
//! Das Fenster startet nicht zur vollen Stunde, sondern mit der ersten Anfrage - und laeuft dann
//! fuenf Stunden. Wer nach dem Reset nichts tut, verschenkt genau diese Zeit. Eine Aufgabe hier
//! ist deshalb nichts anderes als ein vorbereiteter erster Request: sie liegt bereit und wird in
//! dem Moment abgeschickt, in dem wieder Kontingent da ist.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::usage::Usage;

/// Mindestabstand zwischen zwei Laeufen derselben Aufgabe.
///
/// Nach einem Lauf wird das dann geltende `resets_at` gemerkt; genau daran erkennt die
/// Faelligkeit, dass dieses Fenster schon bedient ist. Scheitert die Auslastungsabfrage nach dem
/// Lauf, bleibt der gemerkte Wert alt - und ohne diese Bremse liefe die Aufgabe sofort erneut.
pub const MIN_RERUN_GAP_SECONDS: u64 = 15 * 60;
pub const DEFAULT_TIMEOUT_MINUTES: u64 = 60;
pub const DEFAULT_RESUME_PROMPT: &str = "Mach da weiter, wo du aufgehoert hast.";
/// Laenger ist kein Titel mehr, sondern der Auftrag selbst.
const TITLE_MAX_CHARS: usize = 70;

/// Was die Aufgabe an Claude schickt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "art", rename_all = "snake_case")]
pub enum JobKind {
    /// Ein neuer Auftrag in einem Arbeitsverzeichnis.
    Prompt { text: String },
    /// Eine bestehende Sitzung wird fortgesetzt, statt bei null anzufangen.
    Resume { session_id: String, text: String },
}

impl JobKind {
    pub fn text_is_empty(&self) -> bool {
        match self {
            Self::Prompt { text } | Self::Resume { text, .. } => text.trim().is_empty(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Job {
    pub id: String,
    pub title: String,
    #[serde(flatten)]
    pub kind: JobKind,
    pub cwd: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settings: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Eine unbeaufsichtigte Aufgabe kann keine Rueckfrage beantworten; ohne das Kennzeichen
    /// bliebe sie an der ersten Berechtigungsfrage stehen, bis das Zeitlimit sie abraeumt.
    #[serde(default = "yes")]
    pub skip_permissions: bool,
    #[serde(default = "default_timeout")]
    pub timeout_minutes: u64,
    /// Wiederkehrend: laeuft in jedem neuen Fenster erneut. Sonst nur einmal.
    #[serde(default)]
    pub repeat: bool,
    #[serde(default = "yes")]
    pub enabled: bool,
    pub created_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_run_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_status: Option<String>,
    /// `resets_at` des Fensters, in dem diese Aufgabe zuletzt lief. Beim Anlegen wird das
    /// *laufende* Fenster eingetragen: die Aufgabe soll die begonnene Arbeit des Nutzers nicht
    /// stoeren, sondern das naechste Fenster eroeffnen.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_window: Option<DateTime<Utc>>,
}

fn yes() -> bool {
    true
}

fn default_timeout() -> u64 {
    DEFAULT_TIMEOUT_MINUTES
}

impl Job {
    pub fn new(id: String, kind: JobKind, cwd: PathBuf, created_at: u64) -> Self {
        Self {
            title: default_title(&kind),
            id,
            kind,
            cwd,
            settings: None,
            model: None,
            skip_permissions: true,
            timeout_minutes: DEFAULT_TIMEOUT_MINUTES,
            repeat: false,
            enabled: true,
            created_at,
            last_run_at: None,
            last_status: None,
            last_window: None,
        }
    }

    pub fn text(&self) -> &str {
        match &self.kind {
            JobKind::Prompt { text } | JobKind::Resume { text, .. } => text,
        }
    }

    pub fn session_id(&self) -> Option<&str> {
        match &self.kind {
            JobKind::Resume { session_id, .. } => Some(session_id),
            JobKind::Prompt { .. } => None,
        }
    }

    /// Eine Zeile fuer Listen: was die Aufgabe ist, wo sie laeuft und wie sie ausging.
    pub fn summary(&self) -> String {
        let state = match (self.enabled, self.repeat) {
            (false, _) => "erledigt",
            (true, true) => "wartet, wiederkehrend",
            (true, false) => "wartet",
        };
        let mut text = format!("[{}] {} - {}", self.id, state, self.title);
        if self.session_id().is_some() {
            text.push_str(" (Sitzung wird fortgesetzt)");
        }
        text.push_str(&format!("\n      Ordner: {}", self.cwd.display()));
        if let Some(status) = &self.last_status {
            text.push_str(&format!("\n      Zuletzt: {status}"));
        }
        text
    }
}

/// Kuerzt den Auftrag auf eine Zeile, ohne mitten in ein Zeichen zu schneiden.
pub fn default_title(kind: &JobKind) -> String {
    let text = match kind {
        JobKind::Prompt { text } | JobKind::Resume { text, .. } => text,
    };
    let single_line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if single_line.chars().count() <= TITLE_MAX_CHARS {
        return single_line;
    }
    let mut short: String = single_line.chars().take(TITLE_MAX_CHARS).collect();
    short.push_str(" ...");
    short
}

/// Warum eine Aufgabe jetzt laeuft oder eben nicht.
///
/// Beide Faelle tragen ihre Begruendung: eine Automatik, die nur ihre Treffer meldet, sieht im
/// Stillstand genauso aus wie eine kaputte - und niemand merkt den Unterschied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Readiness {
    Due(String),
    Waiting(String),
}

impl Readiness {
    pub fn is_due(&self) -> bool {
        matches!(self, Self::Due(_))
    }

    pub fn reason(&self) -> &str {
        match self {
            Self::Due(reason) | Self::Waiting(reason) => reason,
        }
    }
}

/// Beide Fenster muessen unter der Schwelle liegen, sonst laeuft der Auftrag in ein Limit.
/// `None` heisst: dagegen spricht nichts.
fn blocking_limit(usage: &Usage, threshold: f64) -> Option<Readiness> {
    if usage.five_hour.utilization >= threshold {
        return Some(Readiness::Waiting(format!(
            "Fuenf-Stunden-Fenster ist voll ({:.0}%, {})",
            usage.five_hour.utilization,
            crate::describe_reset(usage.five_hour.resets_at)
        )));
    }
    if usage.seven_day.utilization >= threshold {
        return Some(Readiness::Waiting(format!(
            "Wochenfenster ist voll ({:.0}%, {})",
            usage.seven_day.utilization,
            crate::describe_reset(usage.seven_day.resets_at)
        )));
    }
    None
}

/// Die Sperre gegen einen Doppellauf. `last_run_at` ist der Zeitpunkt, an dem der letzte Lauf
/// *geendet* hat - waere es der Start, waere die Sperre bei einem langen Lauf schon abgelaufen,
/// bevor er ueberhaupt fertig ist, und genau dann greift sie nicht mehr.
fn rerun_block(last_run_at: Option<u64>, now: u64) -> Option<Readiness> {
    let last_run_at = last_run_at?;
    let waited = now.saturating_sub(last_run_at);
    (waited < MIN_RERUN_GAP_SECONDS).then(|| {
        Readiness::Waiting(format!(
            "letzter Lauf ist erst {} min her; Sperre gegen Doppellauf laeuft {} min",
            waited / 60,
            MIN_RERUN_GAP_SECONDS / 60
        ))
    })
}

/// Wann eine Aufgabe laufen darf.
///
/// Drei Bedingungen, jede aus einem konkreten Grund:
/// 1. Beide Fenster muessen unter der Schwelle liegen.
/// 2. Das Fuenf-Stunden-Fenster muss ein anderes sein als beim letzten Lauf. Nach dem Reset
///    meldet die API gar kein `resets_at` mehr: das Fenster ist ungenutzt, und genau dann ist
///    der beste Startzeitpunkt. Laeuft dagegen noch das Fenster, in dem zuletzt gearbeitet
///    wurde, wartet die Aufgabe - sie soll das laufende Kontingent nicht anbrechen.
/// 3. Der letzte Lauf muss lange genug her sein (siehe `MIN_RERUN_GAP_SECONDS`).
pub fn window_readiness(
    usage: &Usage,
    threshold: f64,
    last_window: Option<DateTime<Utc>>,
    last_run_at: Option<u64>,
    now: u64,
) -> Readiness {
    if let Some(blocked) = blocking_limit(usage, threshold) {
        return blocked;
    }
    if usage.five_hour.resets_at.is_some() && usage.five_hour.resets_at == last_window {
        return Readiness::Waiting(format!(
            "dieses Fenster ist schon bedient ({})",
            crate::describe_reset(usage.five_hour.resets_at)
        ));
    }
    if let Some(blocked) = rerun_block(last_run_at, now) {
        return blocked;
    }
    match usage.five_hour.resets_at {
        None => Readiness::Due(format!(
            "Fenster ist ungenutzt ({:.0}%)",
            usage.five_hour.utilization
        )),
        Some(_) => Readiness::Due(format!(
            "neues Fenster ({:.0}%, {})",
            usage.five_hour.utilization,
            crate::describe_reset(usage.five_hour.resets_at)
        )),
    }
}

/// Wann der Fenster-Ping laufen darf.
///
/// Enger als bei einer Aufgabe: der Ping hat nur einen Zweck, naemlich ein *ungenutztes* Fenster
/// zu eroeffnen. In ein laufendes hineinzufunken wuerde nichts bewirken und trotzdem Kontingent
/// kosten - deshalb entscheidet hier allein, ob die API ein laufendes Fenster meldet.
pub fn ping_readiness(
    usage: &Usage,
    threshold: f64,
    last_run_at: Option<u64>,
    now: u64,
) -> Readiness {
    if let Some(blocked) = blocking_limit(usage, threshold) {
        return blocked;
    }
    if let Some(resets_at) = usage.five_hour.resets_at {
        return Readiness::Waiting(format!(
            "Fenster laeuft noch ({:.0}%, {})",
            usage.five_hour.utilization,
            crate::describe_reset(Some(resets_at))
        ));
    }
    if let Some(blocked) = rerun_block(last_run_at, now) {
        return blocked;
    }
    Readiness::Due(format!(
        "Fenster ist ungenutzt ({:.0}%)",
        usage.five_hour.utilization
    ))
}

pub fn readiness(job: &Job, usage: &Usage, threshold: f64, now: u64) -> Readiness {
    if !job.enabled {
        return Readiness::Waiting("Aufgabe ist abgeschaltet".to_owned());
    }
    window_readiness(usage, threshold, job.last_window, job.last_run_at, now)
}

/// Bewertet jede Aufgabe und liefert *alle* Urteile zurueck, nicht nur die faelligen. Was der
/// Aufrufer davon protokolliert, entscheidet er - verschweigen kann er nichts.
pub fn evaluate<'a>(
    jobs: &'a [Job],
    usage: &Usage,
    threshold: f64,
    now: u64,
) -> Vec<(&'a Job, Readiness)> {
    jobs.iter()
        .map(|job| {
            let verdict = readiness(job, usage, threshold, now);
            (job, verdict)
        })
        .collect()
}

/// Eine Sitzungs-ID landet in einer Kommandozeile; alles ausser dem UUID-Alphabet hat dort
/// nichts zu suchen.
pub fn validate_session_id(id: &str) -> Result<()> {
    if id.is_empty() || id.len() > 128 {
        bail!("Sitzungs-ID muss 1 bis 128 Zeichen lang sein");
    }
    if !id
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        bail!("Sitzungs-ID darf nur A-Z, a-z, 0-9, Minus und Unterstrich enthalten");
    }
    Ok(())
}

pub fn parse_job(data: &[u8], expected_id: &str) -> Result<Job> {
    let job: Job = serde_json::from_slice(data).context("Aufgabe ist kein gueltiges JSON")?;
    if job.id != expected_id {
        bail!(
            "Aufgabe `{expected_id}` traegt den falschen Namen `{}`",
            job.id
        );
    }
    Ok(job)
}

/// Vierstellige laufende Nummer. Sie steht im Dateinamen, im Log und im Menue - eine kurze,
/// stabile Kennung ist dort mehr wert als eine zufaellige.
pub fn next_id(existing: &[Job]) -> String {
    let highest = existing
        .iter()
        .filter_map(|job| job.id.parse::<u32>().ok())
        .max()
        .unwrap_or(0);
    format!("{:04}", highest + 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usage::Bucket;

    fn at(text: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(text)
            .expect("Zeitstempel")
            .with_timezone(&Utc)
    }

    /// Ein laufendes Fenster: etwas verbraucht, Reset datiert.
    fn laufendes_fenster() -> Usage {
        Usage {
            five_hour: Bucket {
                utilization: 20.0,
                resets_at: Some(at("2026-08-04T18:00:00Z")),
            },
            seven_day: Bucket {
                utilization: 30.0,
                resets_at: Some(at("2026-08-09T02:00:00Z")),
            },
        }
    }

    /// Direkt nach dem Reset: nichts verbraucht, kein Reset-Zeitpunkt.
    fn ungenutztes_fenster() -> Usage {
        Usage {
            five_hour: Bucket {
                utilization: 0.0,
                resets_at: None,
            },
            seven_day: Bucket {
                utilization: 30.0,
                resets_at: Some(at("2026-08-09T02:00:00Z")),
            },
        }
    }

    fn job_im_laufenden_fenster() -> Job {
        let mut job = Job::new(
            "0001".to_owned(),
            JobKind::Prompt {
                text: "Baue das Ding".to_owned(),
            },
            PathBuf::from("/tmp"),
            1_000,
        );
        job.last_window = laufendes_fenster().five_hour.resets_at;
        job
    }

    /// Der Kern des Ganzen: wer eine Aufgabe anlegt, waehrend er selbst arbeitet, will nicht,
    /// dass sie ihm sofort das laufende Kontingent wegfrisst - sondern dass sie das naechste
    /// Fenster eroeffnet.
    #[test]
    fn eine_aufgabe_wartet_im_laufenden_fenster_und_startet_nach_dem_reset() {
        let job = job_im_laufenden_fenster();
        let wartet = readiness(&job, &laufendes_fenster(), 98.0, 2_000);
        assert!(!wartet.is_due(), "{wartet:?}");
        assert!(wartet.reason().contains("schon bedient"), "{wartet:?}");

        let faellig = readiness(&job, &ungenutztes_fenster(), 98.0, 2_000);
        assert!(faellig.is_due(), "{faellig:?}");
        assert!(faellig.reason().contains("ungenutzt"), "{faellig:?}");
    }

    /// Nach dem Lauf traegt die Aufgabe das Fenster, das sie selbst eroeffnet hat. Genau daran
    /// erkennt sie, dass sie hier fertig ist - sonst liefe sie im selben Fenster immer weiter.
    #[test]
    fn eine_aufgabe_laeuft_im_selben_fenster_kein_zweites_mal() {
        let mut job = job_im_laufenden_fenster();
        let neues_fenster = Usage {
            five_hour: Bucket {
                utilization: 12.0,
                resets_at: Some(at("2026-08-04T23:00:00Z")),
            },
            ..laufendes_fenster()
        };
        job.last_window = neues_fenster.five_hour.resets_at;
        job.last_run_at = Some(2_000);

        let verdict = readiness(&job, &neues_fenster, 98.0, 2_000 + 6 * 3_600);
        assert!(!verdict.is_due(), "{verdict:?}");
        assert!(verdict.reason().contains("schon bedient"), "{verdict:?}");
    }

    /// Der Fall, den die Bremse abfaengt: die Aufgabe lief, aber die Auslastungsabfrage danach
    /// scheiterte, also blieb `last_window` alt. Ohne die Sperre startete sie sofort erneut.
    #[test]
    fn eine_gerade_gelaufene_aufgabe_startet_trotz_fensterwechsel_nicht_erneut() {
        let mut job = job_im_laufenden_fenster();
        job.last_run_at = Some(2_000);

        let verdict = readiness(&job, &ungenutztes_fenster(), 98.0, 2_000 + 5 * 60);
        assert!(!verdict.is_due(), "{verdict:?}");
        assert!(verdict.reason().contains("Doppellauf"), "{verdict:?}");

        let spaeter = readiness(&job, &ungenutztes_fenster(), 98.0, 2_000 + 16 * 60);
        assert!(spaeter.is_due(), "{spaeter:?}");
    }

    #[test]
    fn eine_frische_aufgabe_startet_im_ungenutzten_fenster_sofort() {
        let job = Job::new(
            "0001".to_owned(),
            JobKind::Prompt {
                text: "los".to_owned(),
            },
            PathBuf::from("/tmp"),
            1_000,
        );
        assert!(readiness(&job, &ungenutztes_fenster(), 98.0, 1_000).is_due());
    }

    #[test]
    fn ein_volles_wochenfenster_haelt_die_aufgabe_zurueck() {
        let job = job_im_laufenden_fenster();
        let usage = Usage {
            seven_day: Bucket {
                utilization: 99.0,
                resets_at: Some(at("2026-08-09T02:00:00Z")),
            },
            ..ungenutztes_fenster()
        };
        let verdict = readiness(&job, &usage, 98.0, 2_000);
        assert!(!verdict.is_due(), "{verdict:?}");
        assert!(verdict.reason().contains("Wochenfenster"), "{verdict:?}");
    }

    #[test]
    fn ein_volles_fuenfstundenfenster_haelt_die_aufgabe_zurueck() {
        let job = job_im_laufenden_fenster();
        let usage = Usage {
            five_hour: Bucket {
                utilization: 99.0,
                resets_at: Some(at("2026-08-04T22:00:00Z")),
            },
            ..laufendes_fenster()
        };
        let verdict = readiness(&job, &usage, 98.0, 2_000);
        assert!(!verdict.is_due(), "{verdict:?}");
        assert!(verdict.reason().contains("voll"), "{verdict:?}");
    }

    #[test]
    fn eine_abgeschaltete_aufgabe_laeuft_nie() {
        let mut job = job_im_laufenden_fenster();
        job.enabled = false;
        let verdict = readiness(&job, &ungenutztes_fenster(), 98.0, 9_000);
        assert!(!verdict.is_due(), "{verdict:?}");
        assert!(verdict.reason().contains("abgeschaltet"), "{verdict:?}");
    }

    /// Jede Aufgabe bekommt ein Urteil, auch die wartenden - sonst waere die Automatik im
    /// Journal nicht von einem Ausfall zu unterscheiden.
    #[test]
    fn die_bewertung_liefert_zu_jeder_aufgabe_eine_begruendung() {
        let jobs = vec![job_im_laufenden_fenster(), {
            let mut zweite = job_im_laufenden_fenster();
            zweite.id = "0002".to_owned();
            zweite.enabled = false;
            zweite
        }];
        let urteile = evaluate(&jobs, &ungenutztes_fenster(), 98.0, 9_000);
        assert_eq!(urteile.len(), 2);
        assert!(urteile.iter().all(|(_, v)| !v.reason().is_empty()));
        assert_eq!(urteile.iter().filter(|(_, v)| v.is_due()).count(), 1);
    }

    #[test]
    fn nummern_zaehlen_hoch_und_bleiben_vierstellig() {
        assert_eq!(next_id(&[]), "0001");
        let mut job = job_im_laufenden_fenster();
        job.id = "0009".to_owned();
        assert_eq!(next_id(&[job]), "0010");
    }

    #[test]
    fn ein_titel_wird_gekuerzt_und_einzeilig() {
        let kind = JobKind::Prompt {
            text: format!("erste Zeile\nzweite Zeile {}", "x".repeat(120)),
        };
        let title = default_title(&kind);
        assert!(!title.contains('\n'));
        assert!(title.chars().count() <= TITLE_MAX_CHARS + 4, "{title}");
        assert!(title.starts_with("erste Zeile zweite"), "{title}");
    }

    #[test]
    fn eine_sitzungs_id_mit_sonderzeichen_wird_abgelehnt() {
        assert!(validate_session_id("fa085960-9718-4f19-83f1-a2b2d718c18f").is_ok());
        assert!(validate_session_id("../../etc/passwd").is_err());
        assert!(validate_session_id("id; rm -rf /").is_err());
        assert!(validate_session_id("").is_err());
    }

    #[test]
    fn eine_gespeicherte_aufgabe_bleibt_beim_lesen_dieselbe() {
        let mut job = job_im_laufenden_fenster();
        job.kind = JobKind::Resume {
            session_id: "abc-123".to_owned(),
            text: DEFAULT_RESUME_PROMPT.to_owned(),
        };
        job.last_status = Some("erfolgreich (3 min)".to_owned());
        let data = serde_json::to_vec(&job).expect("schreiben");
        assert_eq!(parse_job(&data, "0001").expect("lesen"), job);
        assert!(parse_job(&data, "0002").is_err());
    }

    /// Aeltere Dateien kennen die spaeter ergaenzten Felder nicht; sie duerfen dadurch nicht
    /// unlesbar werden - und eine Aufgabe ohne Kennzeichen ist aktiv, nicht heimlich aus.
    /// Der Fall, den das Merge-Gate gefunden hat: eine Aufgabe laeuft laenger als die Sperre.
    /// Zaehlte sie ab dem *Start*, waere sie beim Laufende schon abgelaufen - und ein
    /// gescheiterter Fensterabgleich haette die Aufgabe sofort erneut gestartet.
    #[test]
    fn die_sperre_zaehlt_ab_dem_ende_eines_langen_laufs() {
        let mut job = job_im_laufenden_fenster();
        let start = 2_000;
        let dauer = 40 * 60;
        job.last_run_at = Some(start + dauer);

        let direkt_danach = readiness(&job, &ungenutztes_fenster(), 98.0, start + dauer + 60);
        assert!(!direkt_danach.is_due(), "{direkt_danach:?}");
        assert!(
            direkt_danach.reason().contains("Doppellauf"),
            "{direkt_danach:?}"
        );

        let spaeter = readiness(&job, &ungenutztes_fenster(), 98.0, start + dauer + 16 * 60);
        assert!(spaeter.is_due(), "{spaeter:?}");
    }

    /// Der Ping hat nur einen Zweck: ein ungenutztes Fenster eroeffnen. In ein laufendes
    /// hineinzufunken kostet Kontingent und bewirkt nichts.
    #[test]
    fn der_ping_feuert_nur_in_ein_ungenutztes_fenster() {
        let laeuft = ping_readiness(&laufendes_fenster(), 98.0, None, 9_000);
        assert!(!laeuft.is_due(), "{laeuft:?}");
        assert!(
            laeuft.reason().contains("Fenster laeuft noch"),
            "{laeuft:?}"
        );

        let frei = ping_readiness(&ungenutztes_fenster(), 98.0, None, 9_000);
        assert!(frei.is_due(), "{frei:?}");
    }

    /// Bleibt das Fenster nach einem Ping ungenutzt - er hat es also nicht eroeffnet -, wird
    /// nicht sofort nachgefeuert, sondern erst nach der Sperre.
    #[test]
    fn ein_wirkungsloser_ping_wird_nicht_sofort_wiederholt() {
        let gerade_gepingt = ping_readiness(&ungenutztes_fenster(), 98.0, Some(9_000), 9_060);
        assert!(!gerade_gepingt.is_due(), "{gerade_gepingt:?}");
        assert!(
            ping_readiness(&ungenutztes_fenster(), 98.0, Some(9_000), 9_000 + 16 * 60).is_due()
        );
    }

    #[test]
    fn ein_volles_limit_haelt_auch_den_ping_zurueck() {
        let usage = Usage {
            five_hour: Bucket {
                utilization: 0.0,
                resets_at: None,
            },
            seven_day: Bucket {
                utilization: 99.0,
                resets_at: Some(at("2026-08-09T02:00:00Z")),
            },
        };
        let verdict = ping_readiness(&usage, 98.0, None, 9_000);
        assert!(!verdict.is_due(), "{verdict:?}");
        assert!(verdict.reason().contains("Wochenfenster"), "{verdict:?}");
    }

    #[test]
    fn eine_datei_ohne_die_neueren_felder_bleibt_lesbar() {
        let data = br#"{"id":"0001","title":"alt","art":"prompt","text":"tu was",
                        "cwd":"/tmp","created_at":10}"#;
        let job = parse_job(data, "0001").expect("lesen");
        assert!(job.enabled);
        assert!(job.skip_permissions);
        assert_eq!(job.timeout_minutes, DEFAULT_TIMEOUT_MINUTES);
        assert_eq!(job.text(), "tu was");
    }
}
