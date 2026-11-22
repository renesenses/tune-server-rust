Tune 1.0.0-rc13, der dreizehnte Release-Kandidat von Tune 1.0. Danke an Gros Bidon, r5yk5, Tades, cpadiou77, Pierre, Daniel LEVY, Lio, sashasha, ajm, Jean Valjean, Semakawa, Jon Rotaetxe, hifi25nl, aqs, Christopher J, FabienM, Begastor und Régis für ihre Meldungen und Tests, an alec_eiffel für den Smart Upsampler, an cometdom für Qobuz und an Levente für das Design.

## Neuheiten

- **Upsampler**: Der Smart Upsampler von Rémi (alec_eiffel) spielt mit demselben Pegel wie ohne ihn, und eine geänderte Einstellung ist sofort zu hören.
- **Wiedergabe und Ausgänge**: Sendspin und Devialet AIR erhalten den bearbeiteten Klang (Equalizer, Plugins, Upsampler). Der Sleep-Timer kann den Rechner ausschalten; Mediensteuerungen des Desktops unter Linux.
- **Crossfeed**: kostenlos, mit seinen Einstellungen und Voreinstellungen.
- **Faltung**: eine Impulsantwort-Datei pro Abtastrate.
- **DSD**: DSD von einem UPnP-Server wird auf anspruchsvollen Netzwerkplayern nativ gespielt, und zwei DSD-Titel folgen lückenlos aufeinander.
- **Bibliothek**: Ihre persönlichen Tags werden zu Tune-Tags; mehrere Alben für eine Sammelaktion auswählen; Filter nach Instrument.
- **Veränderte Quelle** ersetzt „Zweifelhaftes Hi-Res“, für alle Formate, mit der gemessenen Grenzfrequenz.
- **Wellenform** feiner, auch für CUE-, Qobuz- und TIDAL-Titel (als Option).
- **Warteschlange**: Alben lassen sich einklappen und mit einer Geste verschieben.
- **Start**: 5 Playlists „Weil du … magst“ und die Widgets Meistgehörte Künstler, Häufig gehört, Später hören und Weiterhören.
- **Dienste**: TIDAL-Seite „Für dich“; 200 Qobuz-Neuerscheinungen pro Genre.
- **Tune Circle**: Die Mitglieder eines Kreises sehen sich gegenseitig, wenn sie zustimmen, und die Ansicht Welt zeigt alle Kreise.
- **Weboberfläche**: Türkisch, Authentifizierung in der Oberfläche aktivierbar und Verlauf pro Titel oder pro Wiedergabe.
- **Tune Client (Desktop-App)**: findet die Server im Netzwerk selbst und steuert den lokalen Server.
- **Windows**: Start von Tune mit Windows, vom Installationsprogramm angeboten.
- **iOS-App**: vollständigeres CarPlay über 5G, die Seitenleiste ist auf dem iPad zurück.

## Verbesserungen

- **Übergänge**: ReplayGain „Auto“ behält den Album-Gain zwischen zwei Titeln desselben Albums; AAC, Opus und CUE-Titel laufen lückenlos.
- **Wiedergabe und Ausgänge**: dieselbe Lautstärkekurve für AirPlay 1 und 2; ein Radio startet auf einem Netzwerkplayer schneller.
- **Windows**: Im exklusiven WASAPI-Modus wechselt Tune zur anderen Familie von Abtastraten, statt den Titel zu stoppen.
- **Devialet AIR**: Lautstärke in Schritten von 1 dB, bis 192 kHz.
- **DSD**: Die Erkennung von nativem DSD ist vorsichtiger und wird neu bewertet.
- **Große Bibliotheken**: Ordner öffnet sich ohne Wartezeit, und Hintergrundaufgaben blockieren den Rest nicht mehr.
- **Dienste**: eigener Google-Anmeldeclient von Tune für YouTube; Spotify und TIDAL robuster während der Wiedergabe.
- **Weboberfläche**: genauerer parametrischer Equalizer und Bestätigung vor dem Löschen.
- **iOS-App**: Der Sperrbildschirm steuert die Zone auch im Stillstand; ohne DAC regelt der Schieberegler die Lautstärke von iOS.
- **Update und Diagnose**: Das Update ersetzt auch die mit Tune gelieferten Programme; Fehlerberichte verbergen die Namen.

## Fehlerbehebungen

- **Wiedergabe und Ausgänge**: Das Fortsetzen funktioniert nach einer Pause, die den DAC freigegeben hatte; „Alle wiederholen“ verwechselt einen kurzen Stopp nicht mehr mit dem Ende eines Titels; unter Windows wird ein nach Tune eingeschalteter DAC gefunden.
- **Zonen**: Ein bereits verknüpftes Gerät nennt seine Zone; mit OAAT öffnet ein Fortsetzen die Sitzung neu, statt stumm zu bleiben.
- **DLNA, UPnP und OpenHome**: kein störender Stopp mehr bei Musicbook Source, stabile Stummschaltung, Linn Klimax DS/2 erkannt.
- **AirPlay**: Der AirPlay-1-Ausgang startet nach einem Neustart nicht mehr mit voller Lautstärke.
- **Signalweg**: Er meldet keine Verarbeitung mehr, die nicht stattfindet.
- **Bibliothek**: Die DSD- und CD-Ausgaben desselben Albums werden nicht mehr zusammengeführt; das Schreiben des DR friert den Server nicht mehr ein.
- **Dienste**: Qobuz- und TIDAL-Favoriten mit ihrer Dauer; die Spotify-Rubriken sind nicht mehr doppelt.
- **Radios**: Die FIP-Sender erhalten ihr Logo zurück.
- **Plugins und Crossfeed**: Ein installiertes Plugin wirkt ohne Neustart; Crossfeed ist mit OAAT und im Browser nicht mehr gesperrt.
- **Weboberfläche**: Das Raster scrollt bis zum Ende; das Banner „Der Server antwortet nicht“ verschwindet, sobald er antwortet.

## Sicherheit

- Mehrere Sicherheitshärtungen.
- Protokolle und Fehlerberichte verbergen jetzt die Zugangsdaten, die manche Stream-Adressen enthalten.
- „Pfad ersetzen“ (Einstellungen › Musikordner) prüft jetzt die Ordner und ist von einem anderen Gerät ohne Authentifizierung nicht mehr erreichbar.

## Verhaltensänderungen

- **Tune Circle**: Mit Authentifizierung nur für Administratoren; beim Fernhören schreibgeschützt.
- **YouTube**: Vor dieser Version verbundene Konten müssen einmal neu verbunden werden.
- **Radio am Ende der Warteschlange**: wieder standardmäßig deaktiviert; aktivieren Sie es in der Zone erneut, wenn Sie es gewählt hatten.
- **Equalizer**: Wird der Equalizer ausgeschaltet, bleibt sein Vorverstärker bis „Pegel wiederherstellen“ aktiv.
- **Multiroom**: Eine Gruppe ist bis zu 3 Zonen kostenlos; darüber hinaus Premium. Stereo-Paare bleiben kostenlos.
- **CUE mit INDEX 02**: Titelanfänge werden beim nächsten Scan korrigiert, der Verlauf bleibt erhalten.
- **Tune OS**: Das temporäre Passwort erscheint auf dem Bildschirm der Maschine oder auf der Bootkarte, nicht mehr auf der Anmeldeseite; der Fernzugriff bleibt gesperrt, bis es geändert wurde.
- **Profile und Verlauf**: Mit Authentifizierung verwaltet nur der Administrator die anderen Profile.
- **Pfad ersetzen**: Ohne Authentifizierung an der Maschine, auf der Tune läuft.

## Bitte bei Ihnen prüfen

Diese Änderungen haben die automatischen Tests bestanden, ohne Test auf echter Hardware. Teilen Sie uns mit, was Sie feststellen:

- **Sendspin und Devialet AIR** mit aktiver Verarbeitung.
- **Windows**: Mini-Player, exklusiver WASAPI-Modus, Start mit Windows.
- **AirPlay 1**: Pegel beim Start und zwischen den Titeln.
- **DSD an einen Netzwerkplayer** (Atoll ST200, Eversolo).
- **Linn Klimax DS/2** als OpenHome, **Tune Client unter macOS** und **CarPlay über 5G**.

## Bekannte Probleme

- **DSD per DoP am lokalen Ausgang**: Nach gedämpftem PCM kann es mit voller Aussteuerung spielen. Halten Sie die Lautstärke des Verstärkers niedrig.
- **UPnP nach einer Adressänderung**: Ein Titel wird möglicherweise nicht gespielt; indexieren Sie diesen Server neu.
- **Integritätsprüfung**: Einige Dateien können fälschlich gemeldet werden; löschen Sie nichts allein aufgrund dieses Befunds.
- **Windows ASIO, von einem anderen Server gesteuert**: kein Ton nach Pause und dann Wiedergabe; springen Sie zum nächsten Titel.
- **Nach dem Update**: Wenn sich die Oberfläche nicht ändert, erzwingen Sie ein Neuladen (Cmd+Umschalt+R oder Strg+Umschalt+R).
