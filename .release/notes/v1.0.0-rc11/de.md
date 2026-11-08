Tune 1.0.0-rc11, der elfte Release-Kandidat von Tune 1.0.

## Neuheiten

- **Raumkorrektur**: Ein standardmäßig deaktiviertes Plugin misst den Raum mit Mikrofon (Sweep, Impulsantwort, RT60) oder simuliert seine Bassmoden ohne Mikrofon und schlägt dann ein Ziel und einen automatischen Equalizer vor, der nur absenkt, mit einem Profil pro Zone; ohne Premium wird die Faltung mit der Impulsantwort ausgeblendet.
- **Jugendschutz**: Regeln pro Profil unter Einstellungen › Profile, Ausblenden expliziter Inhalte, Zeitfenster und Hörbudget.
- **Upsampling**: Ein Premium-Plugin für hochwertiges Upsampling wird in den lokalen Ausgang eingebunden, mit Einstellung pro Zone.
- **Erweiterte Abfrage**: Fragen Sie die Bibliothek mit einem Ausdruck ab, demselben, der die intelligenten Sammlungen definiert.
- **Eine Seite pro Album**: Die Versionen eines Albums sind gruppiert und werden auf der Albumseite gewählt; neue Filter für DSD-Qualität und Mehrkanal sowie die Kanalzahl erscheinen.
- **Notizen und Statistiken**: Persönliche Notizen in Markdown und Bibliotheksstatistiken nach Verteilung.
- **Katalog und Spektrogramm**: Exportieren Sie die Bibliothek als druckbaren HTML-Katalog mit den aktuellen Filtern; zeigen Sie das Spektrogramm eines Titels über sein Menü; eine Schaltfläche stoppt die DR-Analyse zwischen zwei Titeln.
- **Weltkarten**: Länderkarte und Länderfilter in der Bibliothek; der Globus zeichnet Grenzen, Küsten und große Flüsse und nennt beim Überfahren das Land.
- **Podcasts**: Fügen Sie einen Podcast über die Adresse seines RSS-Feeds hinzu.
- **TIDAL-Mixe**: Die Reihe „Deine Mixe“ unter Streaming › TIDAL zeigt und spielt die persönlichen Mixe des Kontos.
- **Equalizer-Import**: Importieren Sie eine Equalizer-Datei (AutoEq, Equalizer APO, OPRA) mit Vorschau der Bänder vor dem Anwenden.
- **Biografien**: Links zu Tune-Künstlern und -Alben und zur Quelle; Biografien und Albumnotizen werden bei Bedarf aus Qobuz ergänzt, ohne etwas zu überschreiben und umkehrbar.
- **Playlists**: Ersetzen Sie einen nicht verfügbaren Diensttitel an Ort und Stelle und reparieren Sie die Playlist.
- **Tune Circle**: Schreiben Sie unter Nachrichten an einen ganzen Kreis; allgemeine Kreise und die Zustimmung zum Thread laufen über das Relay.
- **Physische Sammlung**: Cover jeder Disc, Suche in Streamingdiensten, Durchsuchen nach Ordnern, Genres und Stilen.

## Verbesserungen

- **Radios**: Filter nach Mindest-Streamqualität unter „Weitere Filter“, „Meine Radios“ getrennt von „Zuletzt gehört“, Standardstream im Hörland immer abspielbar.
- **Tune tested**: Tune schlägt nach einer Stunde Hören ohne Fehler vor, ein Gerät hinzuzufügen, über eine eigene Schaltfläche; ein bereits katalogisiertes Gerät fügt einen Haushalt hinzu.
- **Bibliothek**: Einstellung „Bibliothek um Favoriten der Dienste erweitert“, standardmäßig aktiv; schnellere Suche nach Wortanfang; die Rastersuche verlangt alle Wörter; Zeitraum Von / Bis auf der Jahreszeitleiste; flüssigeres Scrollen der Titel.
- **Titellisten**: Die Spaltenreihenfolge ist änderbar und wird pro Ansicht gemerkt; die Playlist-Ansicht erhält Cover, Vorschaugröße und kompakten Kopf; Quellen-Badges im Playlist-Manager.
- **Start und Fortsetzen**: Wählen Sie die Bereiche der Hörübersicht; „Weiterhören“ zeigt einen Untertitel pro Art, setzt einen Podcast mit Titel, Cover und Position fort und ohne Duplikate.
- **Verlauf**: Ein Hören zählt erst nach 30 s oder der Hälfte des Titels; „Wiedergabe“ nach Stopp behält das aktuelle Album oder die Playlist; Podcast-Folgen und Radios tragen den Sendernamen.
- **Hörstatistik**: Importierte Wiedergaben (Roon, Plex, m3u) zählen bei den meistgespielten; intelligente Listen und Verlauf zählen wie die Bibliothek; ein Qobuz-Hören zählt nicht mehr für das gleichnamige lokale Album.
- **Crosstalk und Equalizer**: Kurve Trennung / Färbung und Lautsprecherantwort im Einstellungsbildschirm, Zieltrennung ab 1 dB; parametrischer Graph standardmäßig fixiert (Bearbeiten / Fertig) und eine Farbe pro Kanal.
- **Erweiterungen**: Ein Bereich „Plugin-Status“ für den Administrator; ein Katalogeintrag und sein lokales Plugin sind nur noch eine Erweiterung.
- **Benachrichtigungen**: Fehler bleiben bestehen, pausieren beim Überfahren und werden gruppiert; die letzten fest eingebauten Fehlermeldungen sind übersetzt; der Rohtext der Ausnahme ist unter „Details“ eingeklappt.
- **Hintergrundanalysen**: Der Fortschritt wird angezeigt, die Priorität ist niedrig und der Standard nach dem Scan vorsichtig; die Anreicherung setzt dort fort, wo sie war, und wiederholt nach einem 503; „Durchlauf starten“ der Label-Logos wirkt auch in der Pause.
- **Support und Fernzugriff**: Der Fehlerbericht hängt Protokolle an; Link, QR-Code und Kopieren-Schaltfläche sind unter Einstellungen › Fernzugriff zurück.
- **Wiedergabe**: Transcodierungsgründe werden benannt und FLAC wird unverändert geliefert, wenn nichts zutrifft; Weiter und Zurück überspringen vom Dienst abgelehnte Titel.

## Fehlerbehebungen

- **AirPlay 2**: Die Schaltfläche „Pair“ koppelt über die AirPlay-2-Komponente oder nennt die Ursache; das Warten auf den PIN blockiert die Wiedergabe nicht mehr und das Schließen des Fensters bricht es ab; die Sitzung öffnet sich nach einer Pause neu; die Position läuft weiter und eine Pause läuft nicht mehr in Schleife; die Komponente gibt es für 64-Bit-Raspberry-Pi.
- **DLNA und OpenHome**: Lückenlose DSD-Titel, schnelleres „Weiter“, eine am Gerät ausgelöste Pause wird respektiert, der eigene Ausgang der Maschine erscheint nicht mehr doppelt, OpenHome-Lautstärke ohne Wartezeit, die von Kazoo geladene Liste bleibt erhalten, schnelleres Play bei TIDAL CD.
- **Lautstärke**: Ein neuer lokaler Ausgang startet nicht mehr mit voller Aussteuerung; die Stummschaltung lässt keine neu gelesene Lautstärke mehr durch; jede Reglerposition ergibt einen eigenen Pegel; WASAPI- und ASIO-Lautstärke ist sofort hörbar; Tune folgt dem Gerät unterhalb der Sicherheitsgrenze.
- **Windows**: Die Verknüpfung öffnet ohne Konsolenfenster; ein DoP, das der Ausgang nicht tragen kann, geht als PCM hinaus; die Netzwerkerkennung passiert die Firewall; eine unlesbare Temperatur drosselt die Analysen nicht mehr; das Update startet sein Umschaltskript beim Beenden.
- **Updates und Lizenz**: Unter Unix werden die neue Binärdatei und die Weboberfläche vor dem Ersetzen der alten geprüft, ein Fehler wird deutlich gemeldet; das Entfernen der Lizenz gibt auch die Maschine bei der Website frei.
- **Bibliothek**: Fotos und Videos eines UPnP-Servers werden nicht mehr aufgenommen; eine FLAC-+-CUE-Box bildet ein einziges Album; DSD- und CD-Ausgabe bleiben zwei Alben; eine selbst erstellte Compilation wird nicht mehr zerlegt; eine gelöschte Playlist wird beim Scan nicht neu angelegt; ein erneutes Einlesen ohne Interpreten-Tag verschiebt das Album nicht mehr nach „Unknown Artist“; das Hinzufügen eines Ordners scannt nur diesen Ordner.
- **Cover und Analysen**: Als letzter Ausweg das größte Bild im Ordner; Fake-Hi-Res-Analyse über die ganze Titeldauer; lückenloses MP3; die Integritätsprüfung erklärt intakte FLAC nicht mehr für abgeschnitten.
- **Streamingdienste**: Die Spotify-Suche liefert Ergebnisse, eine Ablehnung ist nicht mehr stumm und die Favoriten gehen über 50 Titel hinaus; TIDAL › Favoriten friert Tune nicht mehr ein; ein Qobuz-Konto ohne Favoriten liefert schnell seine leere Liste; ein konvertierter Titel startet in Safari.
- **Netzwerk und Speicher**: Die Erkennung setzt fort, wenn das Netzwerk nach Tune ankommt; ein fehlschlagender SMB-Mount sagt warum, auch unter Docker; die Sicherung gelingt unter PostgreSQL; die CD-Extraktion lehnt ein volles oder nicht beschreibbares Ziel ab; Tune überlässt die Squeezebox-Ports einem Lyrion-Server auf derselben Maschine.
- **Bildschirme**: Der Konzert-Bildschirm öffnet sich ohne Lizenz; „Alle trennen“ erscheint wieder; Aktuelle Wiedergabe folgt dem Albumwechsel; eine abgelaufene Streaming-Sitzung behält ihren Tab „Erneut verbinden“; Neuer Wecker bleibt sichtbar.
- **Raspberry Pi 5**: Der Lüfter des Active Cooler läuft wieder an (neues Tune-OS-Image).
- **Sicherheit**: Härtungskorrekturen.
