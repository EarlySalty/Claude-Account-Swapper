# Claude Account Swapper

Speichert mehrere Claude-Code-Logins und wechselt die aktive Authentifizierung ohne erneuten Browser-Login.

## Prinzip

Claude Code nutzt unter Linux genau einen aktiven OAuth-Datensatz in `~/.claude/.credentials.json`. `claude-account` sichert diesen Datensatz unter einem Profilnamen und ersetzt ihn beim Wechsel atomar. Vor jedem Wechsel wird der aktuelle Stand zurueckgesichert, damit von Claude rotierte Tokens nicht verloren gehen.

- kein eigener OAuth-Flow
- keine Passwoerter oder IMAP-Zugaenge
- keine Proxy- oder API-Manipulation
- Credentials bleiben lokal und werden nie ausgegeben
- offene Claude-Code-Sessions verwenden den global aktiven Login bei ihrer naechsten Anfrage, wie nach `claude auth login`

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
| `claude-account switch <name>` | Wechselt atomar zum gespeicherten Account. Alias: `use`. |
| `claude-account list` | Zeigt alle Profile und markiert das aktive. |
| `claude-account status` | Zeigt den von Claude bestaetigten aktiven Account. |
| `claude-account` | Öffnet das vollständige Hauptmenü. |

Profilnamen duerfen Buchstaben, Zahlen, `.`, `-` und `_` enthalten.

## Sicherheit

Die Profile liegen standardmaessig unter `~/.claude-account-switcher/accounts/`. Verzeichnisse erhalten Modus `0700`, Credential- und Statusdateien Modus `0600`. Schreibvorgaenge erfolgen ueber eine temporaere Datei mit `fsync` und atomarem Rename; parallele Switcher-Aufrufe werden per Dateilock serialisiert.

Ein unbekannter aktiver Login wird niemals still ueberschrieben. Der Switcher verlangt zuerst `claude-account save <name>`. Auch ungueltige Ziel-Credentials und fehlgeschlagene Logins veraendern den vorherigen Login nicht.

`ANTHROPIC_API_KEY`, `ANTHROPIC_AUTH_TOKEN` und `CLAUDE_CODE_OAUTH_TOKEN` haben bei Claude Vorrang vor der Credential-Datei. Solange eine dieser Variablen gesetzt ist, verweigert der Switcher den Wechsel statt einen falschen Erfolg zu melden.

Credential-Backups sind aktive OAuth-Geheimnisse. Nicht in Git, Cloud-Sync oder unverschluesselte Backups aufnehmen.

## Grenzen

- Aktuell ist das Tool bewusst Linux-only. macOS speichert Claude-Credentials im Keychain und braucht einen anderen Backend-Adapter.
- Am sichersten wird zwischen zwei Prompts gewechselt, nicht waehrend eine Anfrage laeuft.
- Der Switcher synchronisiert den aktuellen Credential-Stand bei jedem Wechsel. Er kann jedoch keine internen OAuth-Fehler oder Refresh-Races mehrerer Claude-Prozesse beheben.

Die ausfuehrliche Anleitung liegt in [`docs/index.html`](docs/index.html).

## Entwicklung

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
```

Inoffizielles Community-Tool, nicht von Anthropic herausgegeben oder unterstuetzt.
