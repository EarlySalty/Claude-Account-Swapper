# Auftrag: Linux-Session-Orchestrierung

## Nutzerziel

Die fehlenden Linux-Funktionen des verglichenen Multi-Account-Switchers in Claude Account Swapper ergänzen: parallele Accounts, Projektzuordnung, Auswahlstrategien, T3-Session-Verteilung, 98-%-Schwelle im 5h-Fenster und Login-Pflege. Bedienung über Terminal und eine kleine HTML-Oberfläche im Stil der vorhandenen Pyramide. Windows und macOS bleiben ausdrücklich ausgeschlossen.

## Umsetzung

Der bestehende globale Swapper bleibt bestehen. Neue native Sessions verwenden getrennte, offiziell angemeldete Claude-Konfigurationen. Der Launcher verwaltet Account-Sperren und Claude-Session-IDs. Automatische Wechsel finden nur zwischen abgeschlossenen Antworten statt. Die Oberfläche steuert Session-Regeln und Pools, nicht fremde T3-Datenbankeinträge.

## Umfang

Cargo.toml, Cargo.lock, src/, tests/orchestration.rs, web/, docs/orchestrierung.md, README.md, CHANGELOG.md, install.sh, systemd/claude-account-dashboard.service und diese Task-Akte.

Bestehende Testdateien werden nicht entfernt oder ersetzt. Keine Änderung an T3-Laufzeitdaten, fremden Worktrees, Proxy-Konfigurationen oder bestehenden Anmeldungen. Keine automatische Kontoerstellung. Keine Passwort- oder Token-Ausgabe. Keine Kopie eines rotierenden Grants in mehrere aktive native Konfigurationen.

## Abnahme

Die bestehenden Rust-Tests, Formatierung und Clippy müssen erfolgreich sein. Neue lokale Integrationstests prüfen parallele Account-Nutzung, Sperren, 98-%-Wechsel, identische Session-ID nach dem Wechsel, laufende Antworten, wartende Abbrüche, gepufferte Eingaben vor EOF, Projektregeln, Modellkontingente und HTTP-Schutzmaßnahmen.

Ein echter Ende-zu-Ende-Lauf mit der installierten T3-Version und zwei nativen Browser-Anmeldungen ist eine getrennte Aktivierungsabnahme. Er wurde nicht durch den Protokollsimulator ersetzt. Die T3-Bridge wird bis zu diesem Nachweis als noch nicht live abgenommen dokumentiert.

## Betriebsgrenzen

Pro Account ein verwalteter Prozess; verschiedene Accounts parallel. Interaktive Terminal-Prozesse bleiben fest auf ihrem Start-Account. Ein laufender Turn wird nicht für einen Wechsel unterbrochen. 98 % ist eine Entscheidungsschwelle auf Basis periodischer Messungen, keine Garantie gegen eine Limitüberschreitung innerhalb eines langen Turns. Native Projektverläufe müssen zum Account-Wechsel geteilt sein. Nicht über den Launcher gestartete T3-Prozesse werden nicht nachträglich übernommen.
