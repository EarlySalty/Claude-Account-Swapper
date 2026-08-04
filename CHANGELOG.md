# Changelog

## #11 — Das Fünf-Stunden-Fenster startet von selbst, und Aufträge warten darauf

Das Fenster beginnt nicht mit dem Reset, sondern mit der ersten Anfrage danach. Wer nachts um vier nicht am Rechner sitzt, verschiebt damit seine ganzen fünf Stunden — und musste sich bisher selbst einen Wecker stellen, um irgendetwas zu tippen.

Ab jetzt kann der Hintergrunddienst das übernehmen. Sobald die Auslastung kein laufendes Fenster mehr meldet, schickt er von selbst eine kurze Nachricht los und eröffnet es damit zum frühestmöglichen Zeitpunkt; danach steht im Protokoll, bis wann das neue Fenster läuft — meldet die Schnittstelle weiterhin keines, wird das als Fehlschlag gemeldet statt stillschweigend übergangen.

Zusätzlich lassen sich Aufträge hinterlegen, die genau dann anlaufen: ein Prompt mit Arbeitsverzeichnis, einmalig oder in jedem neuen Fenster. Auch eine frühere Sitzung lässt sich auswählen — sie wird dann im Hintergrund fortgesetzt, sobald wieder Kontingent da ist. Ein Auftrag, der bei laufendem Fenster angelegt wird, nimmt niemandem das gerade genutzte Kontingent weg, sondern wartet auf das nächste. Läuft er, steht der Grund im Protokoll; wartet er, ebenfalls. Ein gescheiterter Lauf verbraucht den Auftrag nicht, sondern behält ihn mit der Begründung.

Beides wird im Menü unter „Automatik und Aufgaben" ein- und ausgeschaltet und wirkt sofort, ohne den Dienst neu zu starten. Das Menü kennt außerdem endlich die Auslastungsübersicht und die eigenen Grenzen pro Account, die es bisher nur auf der Kommandozeile gab.

## #10 — Der automatische Wechsel wird jetzt eingeschaltet, statt vorausgesetzt

Ein Wechsel zieht jede laufende Sitzung mit. So etwas gehört nicht standardmäßig an — auch wenn es meistens das Richtige tut.

Der Hintergrunddienst wechselt ab jetzt nur noch, wenn es ausdrücklich eingeschaltet ist. Ohne diesen Schalter fragt er die Auslastung gar nicht erst ab; von Hand bleiben Übersicht und Wechsel unverändert nutzbar. Beim Start steht im Protokoll, welcher der beiden Zustände gerade gilt.

## #9 — Jeder Account kann seine eigene Grenze bekommen

Bisher galt für alle Accounts dieselbe Schwelle. Wer einem Account einen Teil seines Kontingents freihalten wollte — etwa das Wochenlimit für etwas Bestimmtes — konnte das nicht ausdrücken.

Ab jetzt lässt sich pro Account festlegen, ab wie viel Prozent er verlassen wird, getrennt für das Fünf-Stunden- und das Wochenfenster. Eine gesetzte Grenze schützt den Account auch davor, als Ziel gewählt zu werden — sonst wäre die Reserve wertlos. Ist am Ende kein Account mehr unter seiner Grenze, wird eine Reserve angebrochen, statt die Arbeit stehenzulassen; im Protokoll steht dann genau das. Wer das nicht will, macht die Grenze hart: dann wird lieber gewartet.

Gesetzt wird sie mit `claude-account limit <name> --five-hour 80 --seven-day 50`, entfernt mit `--clear`. Erneutes Speichern eines Accounts lässt die Grenzen unangetastet.

Außerdem fragt die Prüfung jetzt sparsamer: solange der aktive Account unter seiner Grenze liegt, werden die anderen gar nicht erst abgerufen. Zahlen werden gemerkt und tragen bis zu einer halben Stunde weiter, falls die Abfrage einmal blockiert — mit Altersangabe in jeder Zeile, die darauf beruht.

## #8 — Wechselt von selbst, bevor das Limit zuschlägt

Bisher merkte man erst mitten in der Arbeit, dass das Fünf-Stunden-Kontingent aufgebraucht war — und musste dann von Hand suchen, welcher der gespeicherten Accounts überhaupt noch Luft hat.

Der Hintergrunddienst liest jetzt einmal pro Minute die Auslastung aller gespeicherten Accounts und wechselt ab 98 Prozent selbstständig auf den mit den meisten freien Kontingenten. Ist gerade keiner frei, wird der genommen, dessen Limit am frühesten wieder zurückgesetzt wird; das Wochenlimit zählt dabei genauso wie das Fünf-Stunden-Fenster. Jede Entscheidung steht mit ihren Zahlen im Protokoll, auch die gegen einen Wechsel.

Die Auslastung lässt sich mit `claude-account usage` jederzeit ansehen, die Entscheidung mit `claude-account auto --dry-run` durchspielen. Abschalten: `watch --no-auto-switch`.

## #7 — Fehlgeschlagene Prüfungen sagen jetzt, woran es lag

Scheiterte die Prüfung eines gespeicherten Zugangs, stand in der Meldung nur „Claude endete mit exit status: 1". Ein erschöpftes Kontingent, ein toter Zugang und eine gestörte Verbindung sahen damit identisch aus — und keiner davon war behebbar, ohne zu raten.

Die Meldung enthält ab jetzt Claudes eigene Begründung. Sieht ein Teil davon wie ein Zugangsschlüssel aus, wird er vorher entfernt, damit nichts Vertrauliches im Protokoll landet.

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
