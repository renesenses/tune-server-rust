Tune 1.0.0-rc14 enthält den gesamten Inhalt der Vorabversionen rc13.90 und rc13.91; diese Hinweise beschreiben, was sich seit der rc13.91 geändert hat. Danke an die Tester: Viele dieser Punkte stammen aus Ihren Rückmeldungen.

## Neuheiten

- **TESS, Voreinstellungen Speichern und Neu laden**: Sobald eine Einstellung von der gewählten Voreinstellung abweicht, zeigt TESS „geändert“ an. **Speichern** überschreibt die ursprüngliche Voreinstellung, **Neu laden** kehrt zu ihr zurück. Eine mit Tune ausgelieferte Voreinstellung wie „Pure“ wird nicht überschrieben: „Speichern unter…“ legt eine Kopie an.
- **TESS, DSD-Voreinstellung sofort angewendet**: Eine Voreinstellung, die das DSD-Ziel ändert (zum Beispiel DSD128 → DSD256) und im Modus „Sofort anwenden“ geladen wird, startet den Titel an seiner Position neu, statt auf den nächsten Titel zu warten. Upsampling ist eine Premium-Funktion.
- **Headroom**: Automatisch oder von Hand von −12 bis 0 dB in Schritten von 0,5 dB. Das Panel zeigt den tatsächlich angewendeten Wert oder sagt, warum er nicht angewendet wird.
- **Headroom beim Upsampling nach DSD**: Der manuelle Headroom wird nicht mehr doppelt angewendet (−12 dB ergaben −24 dB, jetzt sind es −15 dB, davon −3 dB für den DSD-Modulator, in einer eigenen Zeile angezeigt).
- **Raummessung mit Führung auf 75 dB(C)**: Mit einem kalibrierten Mikrofon zeigt der Bildschirm den gemessenen Pegel, seine Abweichung von 75 dB(C) und eine Lautstärkeanweisung. Die Regel ist vorsichtig, da die Umrechnung noch nicht mit einem Kalibrator geprüft wurde: Tune rät nie, über geschätzte 80 dB(C) hinauszugehen, und schaltet das Signal bei 90 dB SPL ab. Die mit Mikrofon gemessene Korrektur ist Premium; die ohne Mikrofon berechnete bleibt kostenlos.
- **Gehörschutz**: Das Abwählen eines Equalizers, der Wechsel zu PURE oder Bypass all oder das Entfernen einer Faltung, eines Crossfeeds oder einer Mono-Zusammenführung lässt den Pegel nicht mehr schlagartig ansteigen: Er kehrt mit einer Blende von etwa einer halben Sekunde auf den vorherigen Pegel zurück. Haben Sie die Lautstärke inzwischen erhöht, leuchtet die Anzeige „Pegel reduziert“ auf, und „Pegel wiederherstellen“ gibt den Überschuss frei. Das gilt auch an den Netzwerkausgängen (UPnP, OAAT).
- **Getreuer Signalweg**:
  - An einem Netzwerkausgang folgt er der tatsächlichen Reihenfolge des Datenstroms, bis zur Lautstärke des Players;
  - die Stufe eines Impulsantwort-Filters heißt „Faltung“;
  - der Filter behält den Namen der importierten .wav- oder .zip-Datei;
  - nach einer Änderung des DSD-Ziels zeigt er die tatsächlich gespielte DSD-Rate und kündigt die nächste an;
  - das Fenster wird grün: Orange bleibt echten Warnungen vorbehalten.
- **Faltung**: Das Panel zeigt die Kurve des geladenen Filters, Kanal für Kanal, und auf Wunsch die Phase.
- **Bypass all**: Das TESS-Symbol in der Wiedergabeleiste wird abgedunkelt, wenn TESS umgangen ist, und die TESS-Seite zeigt „TESS umgangen“.
- **Vereinheitlichte TESS-Seite**: Akzent in TESS-Grün, aktuelle Zone neben dem Abzeichen, Panel Headroom an einem Netzwerkausgang ausgegraut, und Messung „Ohne Mikrofon (Schätzung)“ oder „Mit Mikrofon (empfohlen)“.
- **Netzwerkausgang**: Die Karte Upsampling sagt, dass das Upsampling dort nicht greift.
- **Vereinheitlichte Oberfläche**: eingebettete Schriften, auch offline; Bibliothek, Live Radio, Medienserver und Konzerte folgen dem neuen Design.

## Fehlerbehebungen

- **Radio an einen Sonos** (und jeden Netzwerk-Player mit Ratenobergrenze): Ein Radiosender in 192 kHz wird mit der Rate geliefert, die der Player akzeptiert. Bisher brach der Player sofort ab, immer wieder.
- **768 kHz**: Die Liste „Max. Rate“ einer Zone bietet 768 kHz und 1.536 kHz an.
- **Bibliothek**: Das Albenraster friert beim Scrollen durch eine große Sammlung nicht mehr ein.
- **Wiedergabe im Browser-Tab**: Das Springen im Titel verschiebt die Wiedergabe tatsächlich, auch von einem anderen Gerät aus.
- **Hilfe**: Das Banner zum Priority-Support erscheint bei einem Premium-Konto nicht mehr.
- **Lizenz**: Das Ablaufdatum wird korrekt gelesen, und Einstellungen › Lizenz sagt, warum die Stufe kostenlos ist.
- **Faltung**: Ein Filter, der bestimmte Frequenzen anhebt, übersteuert nicht mehr.
- **Lange Podcasts**: Eine Folge, deren Hoster die Verbindung regelmäßig trennt, wird nicht mehr abgebrochen.
- **Debian-Paket in einem LXC-Container** (Proxmox): Der Dienst startet auch ohne private Mount-Namespaces.
- **Erster Start**: Der Assistent nennt Dienste ohne Konto nicht mehr „aktiviert“, und er speichert den gewählten Musikordner. Ein leeres „Meine Radios“ öffnet das Verzeichnis.

## Sicherheit und Konten

- **Erneuerte Sitzung**: Mit „Angemeldet bleiben“ (standardmäßig nicht angehakt) erneuert sich die Browsersitzung, solange sie genutzt wird, bis zu 90 Tage. Sonst wie bisher 24 h. Das Avatar-Menü bietet „Von diesem Server abmelden“ an.
- **Administratorzugang wiederherstellen**: Führen Sie `tune-server --reset-admin` (oder `--reinitialiser-admin`) **auf dem Server-Rechner** aus. Der erhaltene Einmalcode, 15 Minuten gültig, wird unter „Passwort vergessen?“ auf diesem Rechner eingegeben; ohne Browser fügen Sie `--password-stdin` hinzu. Offene Sitzungen werden geschlossen. Die Installationsanleitung (Abschnitt 9) nennt den Befehl für jedes System.
- **Profil `admin`**: Das standardmäßig angelegte Administratorprofil heißt jetzt `admin`; ein gewählter Name bleibt erhalten, und `default` wird weiterhin akzeptiert.
- **Administratorpasswort ab der ersten Einrichtung**: Bei einer Neuinstallation lässt der Assistent Sie es anlegen.
- **Server ohne Authentifizierung**: Die Verwaltung wird aus dem lokalen Netzwerk angenommen, nicht mehr nur vom Server-Rechner. **Warnung**: Ohne Authentifizierung können die Geräte im lokalen Netzwerk den Server verwalten; aktivieren Sie sie, wenn Ihr Netzwerk nicht vertrauenswürdig ist. Die Einstellung „Verwaltung nur von diesem Rechner“ stellt die bisherige Regel wieder her.

## Fernbedienungen

- Auf einem geschützten Server fragen Apps, die noch nicht aktualisiert sind, wie bisher alle 24 h erneut nach dem Passwort.

## Bitte bei Ihnen prüfen

- **„Bypass all“ in einer Voreinstellung**: Eine Voreinstellung, die zugleich Bypass all und das DSD-Ziel ändert, startet im Modus „Sofort anwenden“ den Titel an seiner Position neu. Nur am Prüfstand geprüft: Sagen Sie uns, ob der Neustart sauber ist.
