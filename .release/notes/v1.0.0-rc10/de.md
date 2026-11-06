Tune 1.0.0-rc10, der zehnte Release-Kandidat von Tune 1.0.

## Neuheiten

- **Meine physische Sammlung**: Ein neues Plugin vergleicht Ihre Discogs-Sammlung mit Ihrer Bibliothek und den Streamingdiensten, in einem Raster aus Covern, die auf dem Server zwischengespeichert werden. Ein eigener Eintrag erscheint in der Seitenleiste, direkt nach Tune Circle.
- **Mein Design**: Wählen Sie alle Farben der Oberfläche selbst, pro Profil. Auch ein Plugin kann ein Design liefern, und Sie können Ihres an einen Tune-Circle-Kontakt senden, der es mit einem Klick übernimmt.
- **Crosstalk**: Das Plugin zur Verringerung des Übersprechens zwischen Lautsprechern kommt mit seinem Einstellungsbildschirm in den Erweiterungskatalog; in derselben Zone schaltet es Crossfeed ab und zeigt das an.
- **Ordner**: Spielen Sie einen Ordner mit allen Unterordnern ab oder fügen Sie ihn der Warteschlange hinzu, in der richtigen Reihenfolge; der Verzeichnisfilter der Bibliothek steigt in Unterordner hinab und bietet die Rückkehr zur ganzen Bibliothek an.
- **Radio**: Eine Leiste „Weitere Filter“ in Meine Radios und im Verzeichnis ermöglicht die Suche nach Kriterien; vier neue Genres kommen ins Verzeichnis.
- **Radio in der Wiedergabe**: Die Streams eines Senders sind nach Qualität sortiert; Aktuelle Wiedergabe zeigt das Senderlogo und eine Schaltfläche „Meine Radios“; das Empfangsland wird unter Einstellungen › Allgemein festgelegt.
- **Labels**: Die Logos der Plattenlabels werden angezeigt, bereitgestellt von mozaiklabs.fr; der Serverstatus verfolgt den Fortschritt der Albumcover und Labellogos.
- **Lautstärke**: Der Regler ist in dB skaliert, Komfort- und Sicherheitsgrenze erscheinen farbig, und eine Komfortgrenze oberhalb der Sicherheitsgrenze wird abgelehnt.
- **Oxygen und Metadaten**: Ändern Sie ein Feld für ein ganzes gefiltertes Ergebnis; schließen Sie Ordner von der automatischen Metadatenanreicherung aus.
- **Tune Circle**: Zoom auf einen Bereich des Globus in der Weltansicht; Tune-Circle-Playlists haben ihr eigenes Kennzeichen in den Playlist-Filtern; eine Playlist eines Dienstes lässt sich von ihrer Seite aus teilen.
- **Desktop-Widget**: Das Mini-Fenster übernimmt die Mini-Ansicht der Weboberfläche und erhält eine Suche nach Künstlern, Qobuz und TIDAL, mit Vorschaubildern und Tastaturbedienung.
- **AES67-Ausgang**: Ein experimenteller Ausgang zu AES67-kompatiblen Dante- und Ravenna-Audionetzwerken, standardmäßig deaktiviert, findet sich unter Einstellungen › Experimentell.

## Verbesserungen

- **Dieser Browser**: Der nächste Titel wird vor dem Ende vorbereitet, für flüssigere Übergänge, und die Zone „Dieser Browser“ des Servers spielt ohne vorherige Einrichtung.
- **Weiterhören**: Die Karte nennt das fortgesetzte Album, die Playlist oder die Episode, und ein beendetes Hören verschwindet aus der Liste.
- **Verlauf und Top-Titel**: Die Titel eines geöffneten Albums oder einer Playlist laufen vom neuesten zum ältesten; die Top-Titel eines Künstlers ergänzen die, die Sie am meisten hören, aus allen Quellen.
- **Bibliothek**: Die Filter nach DR und nach Originaljahr sind in großen Bibliotheken deutlich schneller; die Spalten der Titelliste lassen sich in der Breite ändern, und jeder Bildschirm merkt sich ihre Breite; die akustische Analyse erklärt ihr Tempo und zeigt ihren Fortschritt.
- **Equalizer**: Freie Eingabe von Frequenz und Q, übernommen am Ende der Eingabe; das Löschen importierter Voreinstellungen verlangt eine Bestätigung; Raumkorrektur und DAC-Kalibrierung wenden das vollständige Equalizer-Profil an.
- **Playlists**: Der Import von .m3u, .m3u8 und .dpl befindet sich im Playlist-Manager und meldet die tatsächliche Zahl der Titel; Importieren, Übertragen und Vergleichen sagen, warum sie scheitern.
- **Cover**: Das eingebettete Cover wird aus allen Tags gelesen, Vorderseite zuerst; die Online-Suche folgt auf das erneute Lesen der Datei; das rechteckige Bild einer Qobuz-Playlist wird vollständig angezeigt.
- **Meldungen**: Die letzten noch französischen Fehlermeldungen sind in die Sprache der Oberfläche übersetzt; ein AirPlay-2-Lautsprecher, der die Verbindung ablehnt, meldet eine fehlende Kopplung; eine von Qobuz abgelehnte Anwendungs-ID wird nicht mehr als falsches Passwort dargestellt.
- **Lizenz**: Der Bildschirm gibt an, dass Premium von Ihrem Mozaiklabs-Konto stammt, ohne einen Schlüssel zu verlangen.
- **Startseite**: „Andere Versionen“ hängt nicht mehr beim Warten auf die Streamingdienste.
- **ASIO**: Wenn die DSD-Unterstützung nicht geprüft werden kann, meldet Tune das und erklärt, was der Ausgang kann.

## Fehlerbehebungen

- **Wiedergabe**: Das Umschalten zwischen PURE und Equalizer startet eine gestoppte Zone nicht mehr neu; das Abwählen von „Standardmäßig wiederholen“ schaltet die Wiederholung der Zone wirklich ab; Übergänge zwischen kompatiblen Titeln bleiben lückenlos.
- **OpenHome**: Pause und Stopp entfernen den vorbereiteten nächsten Titel aus dem Player, und die Endposition bleibt beim Wechsel zum nächsten Titel erhalten.
- **Radio**: Bei unbekanntem Empfangsland wird der gewählte Stream nicht mehr durch einen 96-kbit/s-Stream ersetzt; ein über die Konsole hinzugefügter Sender behält die Verbindung zu seiner Seite; ein einzelner Fehler setzt die Radio-Metadaten nicht mehr aus; „Zuletzt gehört“ zeigt das richtige Datum; die aus Radiofavoriten erstellte Qobuz-Playlist erscheint sofort.
- **Bibliothek und Profile**: Die Filter für Favoriten und Bewertungen folgen dem aktiven Profil; „Meistgehört“ zählt ein Hören nicht mehr mehrfach; die Anreicherung ersetzt kein vorhandenes Genre mehr; UPnP-Titel behalten Jahr und Genres; eine bereits vergebene MusicBrainz-Kennung wird nicht ein zweites Mal gesetzt.
- **Sammlungen und Playlists**: Ein abgelehntes Speichern wird gemeldet, ein fehlerhafter Ordner leert die Liste nicht mehr, und alle Seiten der Playlists werden gelesen.
- **Plugins und Konto**: Das Deinstallieren eines aktiven nativen Plugins deaktiviert es dauerhaft; die erste Verknüpfung mit einem Mozaiklabs-Konto wird dem einzigen Profil zugeordnet, auch ohne E-Mail-Adresse.
- **Sonstiges**: Der Playlist-Import beim Scan sagt, warum er nicht importiert; ein Fehler beim Lesen der Einstellungen wird gemeldet, statt auf Standardwerte zurückzufallen; der Ruhezustand des Computers wird nicht mehr als Hänger gemeldet; die alte Oberfläche überlebt kein Server-Update mehr; die VU-Meter-Leiste läuft zwischen 1280 und 1800 px nicht mehr über; der WASAPI-Modus bleibt bei der Windows-Standardeinstellung sichtbar; Audio wird beim Senden nicht mehr komprimiert.
- **Sicherheit**: Härtungskorrekturen.
