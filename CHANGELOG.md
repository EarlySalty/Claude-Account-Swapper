# Changelog

## #2 — Per Doppelklick bedienen

Der Account-Wechsel erforderte bisher Terminalbefehle und war dadurch unnötig umständlich. Ein Desktop-Starter öffnet nun ein dauerhaftes Terminalmenü zum Speichern, Anmelden, Wechseln und Prüfen der Accounts. Fehler bleiben im Fenster sichtbar und führen zurück ins Menü, statt den Starter zu schließen.

## #1 — Accounts ohne erneuten Browser-Login wechseln

Claude Code konnte lokal nur einen autorisierten Account gleichzeitig halten, wodurch jeder Wechsel erneut durch den Browser-Login fuehrte. Der Switcher speichert jeden einmal autorisierten Account lokal und ersetzt den aktiven Login atomar. Vor dem Wechsel wird der aktuelle Token-Stand zurueckgesichert; unbekannte Logins, ungueltige Profile und fehlgeschlagene Anmeldungen werden nicht ueberschrieben.

Die automatische Pruefung deckt Formatierung, Compiler-Warnungen und alle Wechsel-/Rollback-Tests auf der aktuellen GitHub-Runtime ab.
