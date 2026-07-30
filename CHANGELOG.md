# Changelog

## #6 — Ein Wechsel reißt keine laufende Sitzung mehr ab

Offene Claude-Sitzungen und IDE-Integrationen übernehmen den Account beim nächsten Befehl von selbst — auch mitten in der Arbeit. Genau deshalb traf ein Wechsel auf einen verbrauchten Zugang alle auf einmal: überall stand plötzlich „401 OAuth access token has expired".

Muss ein gespeicherter Zugang vor dem Wechsel erst verlängert werden, wird er jetzt vorher abseits ausprobiert. Trägt er, wird der dabei erneuerte Stand aktiv; trägt er nicht, bricht der Wechsel ab und der bisherige Account läuft unbeschadet weiter. Ein Zugang, der ohnehin noch gilt, wechselt unverändert sofort.

Die Übersicht behauptet außerdem keine Gültigkeit mehr, die es nicht gibt: ein Account, dessen Verlängerung nachweislich scheiterte, wird als „braucht einen neuen Login" geführt.

## #5 — Ungenutzte Accounts bleiben nutzbar

Ein gespeicherter Zugang verfällt rund 30 Tage nachdem er zuletzt benutzt wurde. Ein Account, den man einen Monat lang liegen ließ, war danach nur noch über einen neuen Anmeldevorgang zu retten — auch wenn sein gespeicherter Stand tagesaktuell war.

Der Hintergrunddienst benutzt jetzt jeden Account, der länger als eine Woche nicht dran war, einmal ganz kurz und verlängert ihn dabei. Der gerade angemeldete Account bleibt davon unberührt. Ist ein Zugang bereits verfallen, wird er nicht angefasst, sondern gemeldet.

## #4 — Gespeicherte Accounts laufen nicht mehr ab

Claude Code tauscht den gespeicherten Zugang bei jeder Verlängerung gegen einen neuen aus. Der Switcher hat den Stand eines Accounts aber nur beim Speichern und beim Wechseln gesichert — dazwischen wurde der abgelegte Stand wertlos, und ein späterer Wechsel zurück endete in „Login expired".

Ein Hintergrunddienst sichert Verlängerungen jetzt laufend in das Profil, zu dem sie gehören. Bleibt unklar, welchem Account ein Stand gehört, wird nichts geschrieben und der Grund protokolliert. Die Übersicht zeigt zusätzlich, wann ein Account zuletzt gesichert wurde und wann sein Zugang abläuft, und warnt eine Woche vorher.

## #3 — Wechsel meldet nicht mehr „Login expired"

Nach einem Wechsel blieb der alte Account eingeloggt und Claude Code verlangte einen neuen Login. Grund: Zum Login gehören zwei Dateien, und nur die Token-Datei wurde getauscht — die zwischengespeicherten Kontodaten zeigten weiter auf den alten Account. Dadurch wanderten außerdem die Tokens des neuen Accounts in das Profil des alten.

Der Wechsel setzt die Kontodaten jetzt zurück, sodass Claude sie passend zum neuen Login neu lädt. Alle übrigen Claude-Einstellungen bleiben unangetastet, und die gespeicherten Profile vermischen sich nicht mehr.

Wichtig: Vor dem Wechsel alle offenen Claude-Code-Sessions beenden. Eine laufende Session schreibt ihren Stand beim Beenden zurück und kann den Wechsel sonst wieder überschreiben.

## #2 — Per Doppelklick bedienen

Der Account-Wechsel erforderte bisher Terminalbefehle und war dadurch unnötig umständlich. Ein Desktop-Starter öffnet nun ein dauerhaftes Terminalmenü zum Speichern, Anmelden, Wechseln und Prüfen der Accounts. Fehler bleiben im Fenster sichtbar und führen zurück ins Menü, statt den Starter zu schließen.

## #1 — Accounts ohne erneuten Browser-Login wechseln

Claude Code konnte lokal nur einen autorisierten Account gleichzeitig halten, wodurch jeder Wechsel erneut durch den Browser-Login fuehrte. Der Switcher speichert jeden einmal autorisierten Account lokal und ersetzt den aktiven Login atomar. Vor dem Wechsel wird der aktuelle Token-Stand zurueckgesichert; unbekannte Logins, ungueltige Profile und fehlgeschlagene Anmeldungen werden nicht ueberschrieben.

Die automatische Pruefung deckt Formatierung, Compiler-Warnungen und alle Wechsel-/Rollback-Tests auf der aktuellen GitHub-Runtime ab.
