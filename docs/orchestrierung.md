# Parallele Accounts und T3 Code

Ab Version 0.3 enthält der Linux-Swapper einen zusätzlichen Session-Modus und ein lokales Dashboard. Der bisherige globale Wechsel mit `switch`, `auto`, `watch`, Fenster-Ping und Aufgaben bleibt erhalten. Windows und macOS sind nicht Bestandteil dieser Erweiterung.

## Zwei getrennte Betriebsarten

Der bisherige Swapper ersetzt den gemeinsamen Claude-Login. Der neue Session-Modus startet die offizielle Claude-CLI dagegen mit einem eigenen `CLAUDE_CONFIG_DIR` pro Account. Ein Wechsel im globalen Swapper verändert diese nativen Konfigurationen nicht.

Die nativen Konfigurationen werden einmal über den offiziellen Browser-Login eingerichtet. Bereits vorhandene, getrennt angemeldete Claude-Konfigurationen können angebunden werden. Gespeicherte OAuth-Snapshots werden dafür nicht in mehrere aktive Konfigurationen geklont. Dadurch benutzen parallele Accounts nicht versehentlich denselben rotierenden Refresh-Token.

Pro Account darf ein verwalteter Claude-Prozess gleichzeitig laufen. Zwei Accounts ermöglichen zwei parallele Prozesse; zusätzliche T3-Sessions warten auf einen freien Account. Ein unbeschäftigter T3-Prozess gibt seinen Account nach standardmäßig 60 Sekunden frei. Beim nächsten Auftrag wird derselbe Gesprächsverlauf fortgesetzt. Native Konfigurationen deshalb ausschließlich über die verwalteten Launcher benutzen, nicht gleichzeitig über zusätzliche direkte Claude-Aufrufe.

## Einrichtung

Die Account-Namen müssen bereits mit dem bisherigen Swapper gespeichert sein. `claude-account list` zeigt sie. Bei einer neuen Installation zuerst die Einrichtung im Haupt-README durchführen.

Für gespeicherte Accounts `privat` und `arbeit`:

```bash
claude-account session-login privat
claude-account session-login arbeit
```

Jeder Aufruf öffnet die offizielle Claude-Anmeldung für genau diese native Konfiguration. Anmeldung und Zuordnung sind gesperrt, solange eine verwaltete Session den Account benutzt. Der globale Login bleibt unberührt.

Bereits getrennt angemeldete Konfigurationen lassen sich stattdessen verbinden:

```bash
claude-account bind privat ~/.claude-personal
claude-account bind arbeit ~/.claude-work
```

Der gemeldete Account muss zum gespeicherten Profil passen. Der gemeinsame globale Konfigurationsordner darf nicht als native Session-Konfiguration angebunden werden.

Für gemeinsame Skills, Einstellungen und Gesprächsverläufe:

```bash
claude-account share-setup privat
claude-account share-setup arbeit
```

Geteilt werden vorhandene `projects`, `skills`, `agents`, `commands`, `plugins`, `settings.json` und `CLAUDE.md`. Zugangsdaten und der Identitäts-Cache bleiben getrennt. Bereits vorhandene Inhalte werden nicht ersetzt: Der Befehl nennt dann den betroffenen Pfad, der vor dem Teilen zusammengeführt werden muss. Die Freigabe sollte unmittelbar nach der nativen Anmeldung erfolgen, bevor eigene Verläufe in diesen Verzeichnissen entstehen. MCP-Anmeldungen werden nicht übertragen; erforderliche Verbindungen im jeweiligen Account erneut autorisieren.

## Terminal

```bash
claude-account run privat
claude-account run arbeit
claude-account run privat -- --resume
claude-account run --strategy balanced --pool privat,arbeit
```

Die ersten beiden Befehle können in getrennten Terminals parallel laufen. Andere Terminals und der globale Login bleiben unverändert. Eine interaktive Terminal-Session behält ihren Account bis zum Ende. Der automatische Wechsel innerhalb einer fortlaufenden Session ist im Stream-JSON-Launcher für T3 umgesetzt, nicht durch das gewaltsame Neustarten interaktiver Terminals.

Ein expliziter Terminal-Account kann auch aus einem pausierten Pool manuell gestartet werden. Eine automatische Auswahl verwendet ausschließlich aktivierte Accounts mit frischen, bestätigten Nutzungsdaten.

## Zuordnung pro Projekt und Session

```bash
claude-account map privat ~/projekte/persoenlich
claude-account map auto ~/projekte/gemeinsam --pool privat,arbeit --strategy balanced
claude-account map auto ~/projekte/batch --strategy consume-first
claude-account map
claude-account unmap ~/projekte/batch
```

Ein Aufruf ohne Account, `claude-account run`, übernimmt die Regel des aktuellen Projektverzeichnisses. Unterordner verwenden die nächste übergeordnete Zuordnung. Pfade werden kanonisch aufgelöst; `/arbeit/app` gilt nicht versehentlich für `/arbeit/application`.

T3-Launcher registrieren ihre Claude-Session-ID, das Projektverzeichnis, den Account und ihren Zustand:

```bash
claude-account sessions
claude-account sessions --json
claude-account session <claude-session-id> arbeit
claude-account session <claude-session-id> auto --pool privat,arbeit --strategy best
claude-account session <claude-session-id>
```

Der letzte Befehl entfernt die Session-Ausnahme. Danach gilt wieder die Projektregel. Priorität: Session-Ausnahme, nächster Projektordner, globale Routing-Vorgabe. Eine feste Zuordnung weicht nicht auf einen anderen Account aus. Ein Pool erlaubt Wechsel innerhalb seiner Account-Liste.

## Verteilung und 98-%-Wechsel

```bash
claude-account config --threshold 98
claude-account routing --auto-switch true --strategy balanced
claude-account routing --weekly-threshold 100 --cooldown-seconds 300 --hysteresis 3
claude-account disable arbeit
claude-account enable arbeit
```

Die Routing-Einstellungen gelten für native Sessions; der bisherige globale Auto-Wechsel behält seinen separaten Schalter. `enable` und `disable` steuern den nativen Session-Pool.

| Strategie | Auswahl |
| --- | --- |
| `balanced` | Für neue Sessions zuerst weniger belegte Accounts, danach mehr Restkontingent. |
| `best` | Account mit dem besten verbleibenden Kontingent aus 5h- und Wochenfenster. |
| `consume-first` | Nutzbarer Account mit dem nächsten Wochenreset; ein früher zurücksetzender Account kann auch vor Erreichen der 5h-Grenze gewählt werden. |
| `next-available` | Nächster verfügbarer Account in stabiler Namensreihenfolge. |

Ab 98 % im 5h-Fenster wird vor dem nächsten sicheren Start ein anderer geeigneter Account gewählt. Das Wochenfenster muss ebenfalls frei sein. Bestehende eigene Account-Grenzen werden zusätzlich beachtet; der native Router bricht solche Reserven nicht automatisch an. Cooldown und Sicherheitspuffer begrenzen freiwillige Wechsel. Ein erreichtes Limit hat Vorrang vor dem Cooldown.

Modellspezifische Wochenfenster können zusätzlich geprüft werden:

```bash
claude-account map auto ~/projekte/modell --pool privat,arbeit --model "Name aus der Usage-Anzeige"
```

Das wählt kein anderes Claude-Modell, sondern ergänzt die Kontingentprüfung. Der Modellname muss zur Usage-Antwort passen. Ein fehlendes Modellfenster gilt nicht als freie Quote.

Die Auslastung wird normalerweise etwa alle 30 Sekunden, bei ausgeschöpften Accounts etwa jede Minute gelesen. Fehler führen zu gestaffelten Wiederholungen. Bis zu 120 Sekunden alte Messungen dürfen standardmäßig weiterverwendet werden; ihr Alter bleibt sichtbar. HTTP 401 und 403 nehmen den betroffenen Login sofort aus der Auswahl. Fehlende Daten werden nie als 0 % ausgegeben.

98 % ist eine Umschaltschwelle, keine Garantie gegen jeden Limitfehler: Ein großer bereits laufender Turn kann das verbleibende Kontingent verbrauchen. Laufende Antworten, bestätigte Kontrollanfragen und bekannte Hintergrundaufgaben werden nicht für einen automatischen Account-Wechsel abgebrochen.

## T3 Code

In T3 unter **Settings > Providers** eine Claude-Instanz mit diesem Binary einrichten:

```text
~/.cargo/bin/claude-account-t3
```

Das Feld für `CLAUDE_CONFIG_DIR` leer lassen. Den tatsächlichen Account wählt der Launcher anhand der Session- und Projektregeln. Den originalen Claude-Binary-Pfad nicht durch den Launcher ersetzen; `claude-account-t3` muss weiterhin die echte Claude-CLI finden können.

Der Launcher reicht Stream-JSON, Berechtigungsabfragen und Tool-Rückmeldungen weiter. Bei einem Wechsel startet er Claude mit `--resume` und derselben Session-ID und stellt bestätigte Kontroll-Einstellungen wieder her. Geteilte Projektverläufe sind dafür erforderlich. Ein wartender, noch nicht an Claude gesendeter Auftrag kann abgebrochen werden; dann endet der Launcher, ohne den Auftrag später doch auszuführen.

Bereits laufende T3-Prozesse werden nicht übernommen. Das Dashboard kennt die vom Launcher registrierten Claude-Sessions, nicht sämtliche internen T3-Thread-Titel oder T3-Datenbankeinträge. Für bestehende Arbeit zuerst die Antwort abschließen und die Session anschließend über die neue Provider-Instanz fortsetzen. T3 muss dafür nicht global neu gestartet werden.

**Integrationsstand:** Die Bridge ist gegen eine simulierte Claude-Stream-Schnittstelle getestet. Ein vollständiger Abnahmelauf mit der konkret installierten T3-Version und zwei echten Account-Anmeldungen ist vor dem produktiven Einsatz erforderlich. Die Installation schreibt keine T3-Laufzeitdateien um.

Offizielle T3-Grundlage: [Claude-Provider und getrennte Konfigurationen](https://github.com/pingdotgg/t3code/blob/main/docs/user/providers-claude.md).

## Login-Pflege

Aktive Claude-Prozesse pflegen ihren eigenen nativen Login. Der Monitor nutzt die offizielle CLI für einen kurzen Ping, sobald ein unbenutzter Login aufgefrischt werden muss. Er wartet dabei auf eine freie Account-Sperre. Erfolgreiche und fehlgeschlagene Versuche sind zeitlich begrenzt, damit ein unveränderter Token oder ein Fehler keine Ping-Schleife auslöst. Der vorhandene Ping-Text, das konfigurierte Modell und dessen Zeitlimit werden wiederverwendet.

Die Pflege läuft im bisherigen `watch`-Dienst, im Dashboard und im T3-Launcher. Ein manueller Durchlauf ist möglich:

```bash
claude-account native-refresh
```

Ein bereits widerrufener Login wird nicht als gerettet gemeldet. Dafür ist eine erneute offizielle Anmeldung nötig. Ein über längere Zeit geöffnetes, aber unbenutztes Terminal behält seine Sperre; dessen Login erneuert Claude erst bei der nächsten eigenen Anfrage. Für dauerhaft unbeaufsichtigte Verteilung den T3-Launcher verwenden, der inaktive Prozesse freigibt.

## HTML-Dashboard

```bash
claude-account dashboard
```

Die Oberfläche liegt auf dem ausführenden Rechner unter `http://127.0.0.1:3775/`. Sie zeigt Accounts, Quoten, Reset-Zeiten, Projektregeln und verwaltete Sessions. Änderungen werden ohne Dienstneustart gelesen. Terminal-Sessions sind in der Übersicht sichtbar, aber nicht während ihrer Laufzeit umstellbar.

`./install.sh` installiert zusätzlich `claude-account-dashboard.service`. Ein anderer Port ist über `--port` möglich. Für einen vorhandenen privaten Reverse-Proxy kann dessen exakter Host über `--allow-host` freigegeben werden; der Dienst bindet trotzdem ausschließlich an Loopback. Keine öffentliche Route ohne eigene Zugriffskontrolle einrichten. Eine Route neben der Pyramide wird nicht automatisch in fremde Proxy-Konfigurationen geschrieben.

Die Oberfläche liefert keine OAuth-Tokens, Refresh-Tokens oder Passwörter aus. Schreibzugriffe benötigen eine seitengebundene Anfragekennung; Host und Origin werden geprüft. Externe Scripts, Fonts und Tracker werden nicht geladen.

## Prüfungen

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets --all-features
```

Die neuen Integrationstests verwenden ausschließlich lokale Test-Anmeldungen und einen lokalen Usage-Server. Sie prüfen parallele Accounts, Account-Sperren, Projektregeln, Modellfenster, den Wechsel bei 98 %, die Beibehaltung der Session-ID, laufende Antworten, Abbruch wartender Aufträge, Stream-Ende sowie die HTTP-Schutzmaßnahmen. Das ist kein Ersatz für den beschriebenen echten T3-Abnahmelauf.
