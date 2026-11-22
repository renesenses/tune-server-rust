Tune 1.0.0-rc12, der zwölfte Release-Kandidat von Tune 1.0. Mehrere Bildschirme erscheinen in einer neuen Gestaltung von Levente, Designer aus der Tune-Community.

## Neuheiten

- **Neue Oberfläche**: Tune Circle neu gestaltet (Entdecken, Nachrichten, Geteilte Playlists, Mein Kreis); Albumseite mit Beschreibung, Credits, Details und einem Tab Versionen mit der Katalognummer; Künstlerseite mit großem Porträt, Kennzahlen und Tabs; Einstellungen in einer besser lesbaren Spalte; Serverstatus und Hör-Dashboard, mit Wahl des Zeitraums.
- **Start**: ein neues Widget „Hörübersicht (neue Darstellung)“, hinzuzufügen unter Start › Bearbeiten, mit einem Bereich Zonen; größere Kacheln mit Jahr und Genre.
- **Bibliothek**: Heften Sie die gewünschten Filter über die Schaltfläche Filter an; klassische Musik mit Dirigent, Ensemble / Orchester und Werk; Kanalzahl und Filter Stereo; Karte und Filter nach Land des Künstlers; „Pfad ersetzen“ für einen Musikordner; Erkennung von Fake-Hi-Res, standardmäßig deaktiviert.
- **Technisches Datenblatt eines Titels**: Spektrogramm und Wellenform pro Kanal, auch für die Qobuz- und TIDAL-Titel der Bibliothek.
- **Radio am Ende der Warteschlange**: Wenn die Warteschlange endet, setzt Tune mit ähnlichen Titeln fort, markiert mit „Radio“; einstellbar pro Zone.
- **Sleep-Timer**: Die Musik stoppt zur gewählten Uhrzeit; unter Tune OS kann sich die Maschine auch ausschalten.
- **Playlists**: Anzahl der Wiedergaben und letzte Wiedergabe in jeder Zeile; Kreuz zum Entfernen und „Aus dieser Playlist entfernen“; Entfernen eines Titels aus Ihrer eigenen Qobuz-Playlist.
- **Radio**: Filter Verlustfrei, Hohe Qualität und Hi-Res in einer Zeile; ein Sender aus dem Verzeichnis lässt sich nur für Sie bearbeiten; ein Radio lässt sich zu einer Playlist hinzufügen.
- **Wiedergabe**: natives DSD am lokalen Linux-Ausgang (ALSA); feste PCM-Abtastrate pro Zone; exklusiver Modus pro Zone einstellbar; gelöschte Zonen wiederherstellbar; echte AccurateRip-Prüfung von CDs.
- **DLNA**: durchgehender Stream zwischen den Titeln für Renderer, die zwei Titel schlecht aneinanderfügen, standardmäßig deaktiviert; die Master-Lautstärke einer Gruppe lässt sich von jedem Mitglied aus einstellen.
- **Streaming**: Wahl des TIDAL-Kataloglandes; Qobuz-Neuerscheinungen identisch mit denen des Qobuz-Players; Spotify mit einem Verbindungsassistenten, und Spotify Connect spielt in der gewählten Zone.
- **Signalweg**: Ein Klick auf eine Verarbeitungsstufe öffnet ihre Einstellungen; die gruppierte Wiedergabe zeigt jeden Ausgang.
- **Einstellungen und Oberfläche**: Suche nach Einstellung, Textgröße, Fortschrittsbalken als Wellenform, ein einziges Suchfeld auf allen Bildschirmen, Spracheingabe im Assistenten.

## Verbesserungen

- **Geschwindigkeit**: Intelligente Sammlungen und alphabetische Sprünge sind in einer großen Bibliothek viel schneller; große Listen bremsen den Server nicht mehr.
- **Speicher**: Tune gibt während langer Hörsitzungen freigewordenen Speicher zurück; der Speicher der Transcodierung ist begrenzt.
- **Schnellerer Tonstart**: YouTube startet schneller; TIDAL wartet auf einem lokalen oder OAAT-Ausgang nicht mehr, bis der Titel vollständig heruntergeladen ist.
- **Statistiken**: eine einzige Gesamtzahl von Titeln und Alben auf allen Bildschirmen, ein Eintrag pro Codec, Suche in der gesamten Rangliste.
- **Tune Circle**: Emojis in Nachrichten; wenn das Hören bei einem Kontakt geschlossen ist, sucht Tune denselben Titel in Ihren Quellen.
- **Mobile Apps**: CarPlay spielt direkt auf dem Telefon, mit Sammlungen und einem Tab Radio; Android erhält den TIDAL-Katalog, den Verlauf von Radios und Podcasts und ein Widget Lieblingsradios.
- **Windows**: Ein Banner warnt, wenn die Firewall Tune filtert; WASAPI- oder ASIO-Badge auf lokalen Zonen.
- **Docker**: Der Assistent für SMB-Freigaben funktioniert im Image; das CD-Laufwerk ist im Container nutzbar.
- **Übersetzungen**: korrekte Pluralformen in allen 15 Sprachen.
- **Abhängigkeiten** aktualisiert, darunter das mitgelieferte SQLite 3.54.

## Fehlerbehebungen

- **Weboberfläche**: kein schwarzer Bildschirm mehr nach einem Update der Oberfläche.
- **Stummschaltung**: Eine stummgeschaltete Zone bleibt stumm, wenn ein neuer Titel startet; ein Wecker oder eine Lautstärkegrenze hebt die Stummschaltung nicht mehr auf.
- **Wiedergabe**: Eine beschädigte Datei friert die Zone nicht mehr ein, Tune macht mit dem nächsten Titel weiter; eine beendete Podcast-Folge öffnet sich am Ende nicht mehr erneut.
- **AirPlay 2**: Die Wiedergabe setzt nach einer Pause dort fort, wo sie aufgehört hat, und startet mit der Lautstärke der Zone, nie mit voller Leistung.
- **DLNA und Gruppen**: Die Wiedergabe setzt nach einem Fehler 701 fort; der nächste Titel wird im Voraus vorbereitet; ein Gerät, das nicht antwortet, blockiert die anderen Zonen nicht mehr; Sonos trägt den Namen seines Modells.
- **HQPlayer, Devialet und Diretta**: Position und Status werden verfolgt, automatische Wiederverbindung; der Devialet-Verstärker antwortet wieder; kein falscher Lautstärkeregler mehr bei Diretta.
- **OAAT**: Natives DSD wird angezeigt und ist bit-perfect; DSD1024 wird erkannt, mit Rückfall auf PCM bei einem eingeschränkten Empfänger.
- **Radios**: keine doppelten Sender mehr; ein in Ihrem Land abgelehntes Radio wechselt zu seinem nächsten Stream.
- **Spotify, Qobuz und TIDAL**: Spotify-Playlists und -Favoriten folgen der neuen API; eine Ablehnung durch Qobuz wird nicht mehr als falsche Zugangsdaten dargestellt; das Vorladen bei TIDAL verzögert die Wiedergabe nicht mehr.
- **Bibliothek**: Wiedergaben werden pro Titel gezählt; ein Bindestrich erzeugt keinen falschen Künstler mehr; die Discs einer Box werden besser erkannt; ein Titel ohne Jahr übernimmt das seines Albums.
- **Windows**: Ein fehlender DAC schickt den Ton nicht mehr auf die PC-Lautsprecher; die Ordnerüberwachung umfasst auch Netzwerkfreigaben.
- **PostgreSQL 18**: UPnP-Favoriten funktionieren wieder.

## Sicherheit

- Die Verwaltung von Tune von einem anderen Rechner im Netzwerk erfordert jetzt eine Anmeldung (Tune OS: bis zum neuen Image).
- Webhooks können nicht mehr auf die Tune-Maschine selbst zielen.
- Die Konfiguration von Home Assistant und Hue erfordert Administratorrechte oder die Tune-Maschine selbst; ein auf derselben Maschine wie Tune installiertes Home Assistant wird nicht mehr unterstützt.

## Bekannte Probleme

- **DSD per DoP am lokalen Ausgang**: Es kann nach gedämpftem PCM mit voller Aussteuerung spielen, ohne die Lautstärke zu berücksichtigen. Halten Sie die Lautstärke des Verstärkers niedrig, wenn Sie von einem PCM-Titel zu einem DSD-Titel per DoP wechseln (nächste Version).
- **Integritätsprüfung**: Zahlreiche FLAC-Dateien können fälschlich als beschädigt gemeldet werden; löschen Sie keine Datei allein aufgrund dieses Befunds.
- **Windows ASIO, von einem anderen Server gesteuert**: kein Ton mehr nach Pause und dann Wiedergabe; springen Sie zum nächsten Titel.
- **Nach dem Update**: Wenn sich die Oberfläche nicht ändert, erzwingen Sie ein Neuladen (Cmd+Umschalt+R oder Strg+Umschalt+R). Der erste Start dauert länger, während zwei Indizes erstellt werden.
