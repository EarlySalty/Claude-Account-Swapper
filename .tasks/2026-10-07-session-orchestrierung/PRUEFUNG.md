# Prüfung des Implementierungsstands

## Ausgeführt

- `cargo test --locked --all-targets --all-features --quiet`: 152 Tests erfolgreich. Davon 55 Bibliothekstests, 90 bestehende CLI-Integrationstests und 7 neue Orchestrierungs-Integrationstests.
- `cargo clippy --locked --all-targets --all-features -- -D warnings`: erfolgreich, keine Warnungen.
- `cargo fmt --all`: erfolgreich.
- `node --check web/app.js`: erfolgreich.
- `bash -n install.sh`: erfolgreich.
- `git diff --check`: erfolgreich.

Die neuen Integrationstests benutzen eine lokale Claude-Protokollsimulation und einen lokalen Usage-Endpunkt. Geprüft sind parallele Accounts, gegenseitige Sperren, der 98-%-Wechsel bei erhaltener Session-ID, ununterbrochene laufende Antworten, Abbruch wartender Aufträge, Eingaben vor EOF, Projektzuordnung, Modellfenster und CSRF-/Origin-/Host-Prüfung der Oberfläche. Die Auswahl des ersten freien Accounts kann im parallelen Testlauf variieren; die Tests prüfen deshalb den tatsächlich gewählten Account und dessen späteren Wechsel, statt einen bestimmten Namen vorauszusetzen.

## Kein Live-Nachweis

Ein echter Wechsel zwischen zwei angemeldeten Accounts innerhalb der konkret installierten T3-Version ist nicht durch diese Tests bewiesen. Er benötigt zwei offizielle native Anmeldungen und die Auswahl von `claude-account-t3` als Provider-Binary. Bestehende T3-Laufzeitdateien und laufende Threads wurden nicht verändert. Die HTML-Oberfläche ist durch HTTP-Integrationstests, nicht durch einen dokumentierten visuellen Browser-Abnahmelauf geprüft.
