# Changelog

## #1 — Accounts ohne erneuten Browser-Login wechseln

Claude Code konnte lokal nur einen autorisierten Account gleichzeitig halten, wodurch jeder Wechsel erneut durch den Browser-Login fuehrte. Der Switcher speichert jeden einmal autorisierten Account lokal und ersetzt den aktiven Login atomar. Vor dem Wechsel wird der aktuelle Token-Stand zurueckgesichert; unbekannte Logins, ungueltige Profile und fehlgeschlagene Anmeldungen werden nicht ueberschrieben.
