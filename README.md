# Claude Account Swapper

Speichert mehrere Claude-Code-Logins und wechselt die aktive Authentifizierung ohne erneuten Browser-Login.

## Prinzip

Claude Code nutzt unter Linux genau einen aktiven OAuth-Datensatz in `~/.claude/.credentials.json`. `claude-account` sichert diesen Datensatz unter einem Profilnamen und ersetzt ihn beim Wechsel atomar. Vor jedem Wechsel wird der aktuelle Stand zurueckgesichert, damit von Claude rotierte Tokens nicht verloren gehen.

Zum Login gehoeren zwei Dateien: die Tokens in `~/.claude/.credentials.json` und die Kontodaten, die Claude Code getrennt davon in `~/.claude.json` (`oauthAccount`, `userID`) zwischenspeichert. Der Switcher leert diesen Cache bei jedem Wechsel, damit Claude ihn passend zum neuen Token neu laedt. Bleibt er stehen, meldet Claude Code `Login expired · Please run /login`.

- kein eigener OAuth-Flow
- keine Passwoerter oder IMAP-Zugaenge
- keine Proxy- oder API-Manipulation
- Credentials bleiben lokal und werden nie ausgegeben
- offene Claude-Code-Sessions verwenden den global aktiven Login bei ihrer naechsten Anfrage, wie nach `claude auth login`

Diese Sofortwirkung ist die Staerke des Wechsels und zugleich seine einzige echte Gefahr: gemessen an einer laufenden Session liest Claude Code `.credentials.json` bei **jedem** Request neu. Ein verbrauchter Snapshot wuerde deshalb nicht nur den Wechsel scheitern lassen, sondern jede offene Session und jede IDE-Integration im selben Moment in `401 OAuth access token has expired` reissen.

Deshalb wird ein Snapshot, der vor dem Einsetzen erst verlaengert werden muesste, zuerst in einem eigenen Konfigurationsverzeichnis ausprobiert — dort trifft ein Fehlschlag niemanden. Traegt er, wandert der dabei rotierte Stand live und zugleich ins Profil. Traegt er nicht, bricht der Wechsel ab und der aktive Account laeuft unveraendert weiter. Ein Snapshot mit noch gueltigem Access-Token braucht diese Pruefung nicht und wechselt unveraendert sofort; `claude-account switch <name> --no-check` erzwingt den ungeprueften Tausch.

## Installation

Voraussetzungen: Linux, Rust 1.88 oder neuer und die offizielle Claude-Code-CLI.

Empfohlen: Repository laden und den Desktop-Installer starten:

```bash
git clone https://github.com/EarlySalty/Claude-Account-Swapper.git
cd Claude-Account-Swapper
./install.sh
```

Der Installer legt **Claude Account Swapper** auf dem Desktop und im Anwendungsmenü an. Danach reicht ein Doppelklick. Alternativ kann nur die Kommandozeile installiert werden:

```bash
cargo install --git https://github.com/EarlySalty/Claude-Account-Swapper --locked
```

## Einrichtung

1. **Claude Account Swapper** auf dem Desktop doppelklicken.
2. **Aktuellen Account speichern** wählen und beispielsweise `privat` eingeben.
3. **Neuen Account anmelden** wählen, einen Namen eingeben und einmal den offiziellen Browser-Login abschließen.
4. Danach über **Account wechseln** ohne neuen Browser-Login umschalten.

Das Menü sieht so aus:

```text
[1] Account wechseln
[2] Aktuellen Account speichern
[3] Neuen Account anmelden
[4] Status anzeigen
[5] Beenden
```

Die bisherigen Terminalbefehle bleiben verfügbar:

```bash
claude-account save privat
claude-account login arbeit
claude-account switch privat
claude-account switch arbeit
```

Ohne Argument öffnet `claude-account` dasselbe Hauptmenü wie der Desktop-Starter.

## Befehle

| Befehl | Wirkung |
| --- | --- |
| `claude-account save <name>` | Speichert den aktuell autorisierten Login. |
| `claude-account login <name>` | Startet einmal `claude auth login --claudeai` und speichert den neuen Login. |
| `claude-account switch <name>` | Wechselt atomar zum gespeicherten Account, nachdem der gespeicherte Login geprueft wurde. `--no-check` ueberspringt die Pruefung. Alias: `use`. |
| `claude-account list` | Zeigt alle Profile, den letzten Sicherungsstand und den Token-Ablauf. |
| `claude-account status` | Zeigt den von Claude bestaetigten aktiven Account. |
| `claude-account sync` | Sichert von Claude rotierte Tokens einmalig ins aktive Profil. |
| `claude-account watch` | Macht dasselbe dauerhaft und frischt untaetige Profile auf; laeuft als Hintergrunddienst. `--interval <sekunden>`, Standard 5. |
| `claude-account keepalive` | Benutzt untaetige Profile, damit ihr Login nicht abläuft. `--max-age-days <tage>`, Standard 7. |
| `claude-account` | Öffnet das vollständige Hauptmenü. |

Profilnamen duerfen Buchstaben, Zahlen, `.`, `-` und `_` enthalten.

## Warum Accounts ohne Hintergrunddienst „ablaufen"

Die vollständige Analyse mit allen gemessenen Details steht in [`docs/lebensdauer.html`](docs/lebensdauer.html) — inklusive Token-Lebensdauern, Verhalten bei gescheitertem Refresh und dem Zusammenspiel mehrerer paralleler Sessions. Kurzfassung:

Claude Code verlaengert seinen Login selbststaendig und tauscht dabei den Refresh-Token gegen einen neuen aus. Der alte ist danach verbraucht. Der Switcher hat den Profil-Snapshot frueher nur bei `save`, `switch` und `login` geschrieben — jede Verlaengerung dazwischen fehlte im Profil. Beim naechsten Wechsel zurueck landete der verbrauchte Token wieder in `~/.claude/.credentials.json`, und Claude Code meldete `Login expired · Please run /login`. Der Wechsel selbst funktionierte dabei die ganze Zeit; kaputt war der gespeicherte Stand.

Der Dienst `claude-account watch` schliesst diese Luecke: er beobachtet die aktive Credential-Datei und schreibt jede Verlaengerung sofort in das Profil, zu dem sie gehoert.

### Einrichten

`./install.sh` richtet den Dienst mit ein und startet ihn. Manuell:

```bash
mkdir -p ~/.config/systemd/user
sed "s|__BINARY__|$HOME/.cargo/bin/claude-account|" \
  systemd/claude-account-sync.service >~/.config/systemd/user/claude-account-sync.service
systemctl --user daemon-reload
systemctl --user enable --now claude-account-sync.service
```

### Pruefen

```bash
systemctl --user status claude-account-sync.service
journalctl --user -u claude-account-sync.service -n 20
claude-account list
```

Das Journal enthaelt eine Zeile pro Entscheidung, auch fuer die folgenlosen:

```text
[2026-07-30 11:02:14] Beobachte /home/du/.claude/.credentials.json alle 5s
[2026-07-30 11:02:14] Bereits aktuell: privat
[2026-07-30 18:41:53] Aktualisiert: privat (du@example.com)
[2026-07-30 18:42:03] Uebersprungen: Switcher wird gerade benutzt
```

Eine Ablehnung wird ebenso protokolliert wie ein Erfolg. Meldet der Dienst dauerhaft `ist nicht gespeichert` oder `passt zu mehreren Profilen`, gehoert der aktive Login zu keinem eindeutigen Profil — dann einmal `claude-account save <name>` ausfuehren.

### Untaetige Profile am Leben halten

Ein Snapshot kann perfekt gepflegt und trotzdem wertlos sein: der Refresh-Token laeuft rund 30 Tage nach seiner letzten Nutzung ab. Dagegen hilft nur, das Profil zu benutzen.

Der Dienst frischt darum alle zwoelf Stunden jedes Profil auf, das laenger als sieben Tage nicht gesichert wurde. Es bekommt einen minimalen Request in einem eigenen `CLAUDE_CONFIG_DIR` — der aktiv eingeloggte Account bleibt unberuehrt, und vor dem Schreiben wird geprueft, dass er sich nicht veraendert hat.

```text
[2026-07-30 11:12:53] Untaetige Profile werden nach 7 Tagen aufgefrischt
[2026-07-30 11:12:53] Auffrischung uebersprungen: privat ist aktiv
[2026-07-30 11:12:55] Aufgefrischt: arbeit (du@example.com)
[2026-07-30 11:12:57] Abgelaufen: alt (alt@example.com) braucht einen neuen Login
```

Manuell: `claude-account keepalive`. Abschalten: `watch --no-keepalive`. Ein bereits abgelaufenes Profil wird nicht angefasst, sondern nur gemeldet — retten kann es dann nur noch `claude-account login <name>`.

### Was der Dienst nicht tut

Er schreibt niemals in ein Profil, dessen Zuordnung unklar ist. Widersprechen sich Claudes Identitaets-Cache und der zuletzt vom Switcher gesetzte Account, bleibt alles unveraendert und der Grund steht im Journal. Lieber ein nicht gesicherter Stand als fremde Tokens im falschen Profil.

Kennt Claude die Kontodaten noch nicht — der Zustand direkt nach einem Wechsel — wartet der Dienst, statt auf den Switcher-Status zurueckzufallen. Genau in diesem Fenster koennte eine parallel laufende Session die Datei geschrieben haben. Sobald Claude Code einmal gelaufen ist, wird der aktuelle Stand gesichert; verloren geht dabei nichts, es dauert nur laenger. Der manuelle Befehl `claude-account sync` nutzt den Switcher-Status weiterhin als Rueckfallebene, weil dort ein Mensch weiss, was er gerade gewechselt hat.

Ausschalten: `systemctl --user disable --now claude-account-sync.service`. Ohne den Dienst bleibt der manuelle Weg `claude-account sync` nach jeder laengeren Sitzung.

## Sicherheit

Die Profile liegen standardmaessig unter `~/.claude-account-switcher/accounts/`. Verzeichnisse erhalten Modus `0700`, Credential- und Statusdateien Modus `0600`. Schreibvorgaenge erfolgen ueber eine temporaere Datei mit `fsync` und atomarem Rename; parallele Switcher-Aufrufe werden per Dateilock serialisiert.

Ein unbekannter aktiver Login wird niemals still ueberschrieben. Der Switcher verlangt zuerst `claude-account save <name>`. Auch ungueltige Ziel-Credentials und fehlgeschlagene Logins veraendern den vorherigen Login nicht.

`ANTHROPIC_API_KEY`, `ANTHROPIC_AUTH_TOKEN` und `CLAUDE_CODE_OAUTH_TOKEN` haben bei Claude Vorrang vor der Credential-Datei. Solange eine dieser Variablen gesetzt ist, verweigert der Switcher den Wechsel statt einen falschen Erfolg zu melden.

Credential-Backups sind aktive OAuth-Geheimnisse. Nicht in Git, Cloud-Sync oder unverschluesselte Backups aufnehmen.

## Grenzen

- Aktuell ist das Tool bewusst Linux-only. macOS speichert Claude-Credentials im Keychain und braucht einen anderen Backend-Adapter.
- Am sichersten wird zwischen zwei Prompts gewechselt, nicht waehrend eine Anfrage laeuft.
- Eine laufende Claude-Code-Session haelt `~/.claude.json` im Speicher und schreibt sie beim Beenden komplett zurueck, samt altem Identitaets-Cache. Fuer einen sauberen Wechsel vorher alle offenen Sessions beenden.
- Mit laufendem `claude-account-sync` folgt der gespeicherte Stand den Verlaengerungen von Claude. Ohne den Dienst veraltet er zwischen zwei Wechseln, und ein Rueckwechsel kann `Login expired` melden.
- Der Refresh-Token selbst hat ein Ablaufdatum von etwa 30 Tagen ab der letzten Nutzung. Mit laufendem Dienst wird das durch die Auffrischung abgefangen; ohne ihn braucht ein laenger untaetiges Profil einen neuen Browser-Login. `claude-account list` warnt eine Woche vorher.
- Ein Profil, dessen Token bereits verbraucht ist, laesst sich nicht mehr retten — auch nicht durch die Auffrischung. Der Switcher meldet das, statt es zu verschleiern.
- `claude-account login <name>` macht den neuen Account global aktiv und zieht alle laufenden Sessions mit. Danach gegebenenfalls zurueckwechseln.
- Der Switcher kann keine internen OAuth-Fehler beheben und kein Refresh-Race mehrerer gleichzeitig laufender Claude-Prozesse aufloesen. Schreiben zwei Sessions mit verschiedenen Accounts abwechselnd in dieselbe Credential-Datei, lehnt der Dienst die Zuordnung ab, statt zu raten.

Die ausfuehrliche Anleitung liegt in [`docs/index.html`](docs/index.html).

## Entwicklung

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
```

Inoffizielles Community-Tool, nicht von Anthropic herausgegeben oder unterstuetzt.
