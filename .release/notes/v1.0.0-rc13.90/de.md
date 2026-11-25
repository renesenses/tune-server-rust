Tune 1.0.0-rc13.90: **Vorabversion (Pre-Release) der rc14**, nur für Tester. Dies ist keine veröffentlichte Version: Die automatische Aktualisierung bietet sie nicht an. Danke an alle, die sie testen.

## Neuheiten

- **TESS, die 64-Bit-Verarbeitungskette**: Die gesamte Verarbeitung läuft durchgehend in 64-Bit-Gleitkomma, mit einer einzigen Quantisierung ganz am Ende.
- **Derselbe Weg für alle Ausgänge**: lokaler Ausgang, UPnP/DLNA, AirPlay, Devialet AIR und OAAT laufen über TESS; ein Ausgang, der eine Verarbeitung nicht annehmen kann, sagt das deutlich.
- **Parametrischer Equalizer** mit freien Bändern (Frequenz, Verstärkung, Q), neben dem grafischen Equalizer und dem Kopfhörer-Equalizer (AutoEq-Profile).
- **Faltung**: Filter als .wav, .zip-Archiv oder Convolver-Konfigurationen (.cfg), ein Filter pro Abtastrate, unterbrechungsfreier Wechsel und angegebene Latenz.
- **Raumkorrektur per Messung**: ein FIR-Filter mit gemischter Phase, berechnet aus mehreren Hörpositionen.
- **Upsampling nach DSD** (DSD64, DSD128, DSD256) mit einem Sigma-Delta-Modulator.
- **Ketten-Voreinstellungen**: eine vollständige Kette speichern und abrufen, sofort oder bei der nächsten Wiedergabe.
- **Signalweg**: aus der Wiedergabeleiste, Stufe für Stufe, was wirklich angewendet wird und warum eine Stufe es nicht wird.
- **Native Plugins** in der Kette: Aktivierung im laufenden Betrieb, 64-Bit-Format, Reihenfolge und Anzahl der Kanäle bleiben erhalten.
- **Oberfläche in 20 Sprachen**: Niederländisch, traditionelles Chinesisch, Russisch und Ukrainisch kommen hinzu.
- **Authentifizierung**: eine Schaltfläche „Authentifizierung deaktivieren“ unter Einstellungen › System › Authentifizierung.

## Verbesserungen

- **SACD-ISO**: natives DSD wird an Netzwerkplayer als virtuelles DSF geliefert; per DoP übertragenes DSD meldet sich als DSD.
- **Fernhören**: im 5G-Netz geht der erste Titel als FLAC hinaus.
- **Abwesende Geräte**: nach 30 Tagen automatisch vergessen (einstellbar), ohne je eine Zone zu löschen.
- **Cover+**: bessere Cover, auch für Webradios (ICY-Metadaten).
- **Gruppenwiedergabe**: Versatz pro Zone einstellbar; „Zurück“ in der führenden Zone startet auch die folgenden Zonen neu.
- **Bibliothek und Streaming**: Hintergrunddurchläufe blockieren die Oberfläche nicht mehr, die alphabetische Albumsortierung bleibt erhalten, ähnliche Künstler auf der Künstlerseite eines Dienstes.
- **Ablehnungsmeldungen** sind kürzer und verweisen auf die richtige Einstellung.

## Fehlerbehebungen

- **Die Korrekturen der rc13.1 und rc13.2** sind enthalten: Anmeldung des Administratorkontos, Verknüpfung des mozaiklabs.fr-Kontos aus dem lokalen Netz, Cover bei aktiver Authentifizierung, Aktualisierung unter Windows.
- **UPnP**: Tune indiziert sich nicht mehr selbst; ein indizierter Titel spielt von der aktuellen Adresse seines Servers.
- **Wiedergabe**: 30 s Vorladen ohne verworfene Puffer; ein von der Quelle abgelehnter Sprung verschiebt den Stream nicht mehr.
- **Chromecast** verbindet sich wieder; die erneute AirPlay-Veröffentlichung unterbricht die Wiedergabe nicht mehr.
- **Podcasts**: nicht verfügbare Folgen werden sauber gemeldet.
- **Meldungen aus dem Forum**: Verlauf, Qobuz, Spotify, „Zurück“-Taste, Suche und iPad.

## Verhaltensänderungen

- **Parametrischer Equalizer**: bis zur rc13 kostenlos, gehört er jetzt zu Tune Premium, wie Faltung, Raumkorrektur, Echtzeit-Declick und Plugins von Drittanbietern. Grafischer Equalizer, Kopfhörer-Equalizer und Crossfeed bleiben kostenlos.

## Bitte bei Ihnen prüfen

- **Eine vollständige TESS-Kette** (Equalizer, Crossfeed, Faltung oder Raumkorrektur): Entspricht der Signalweg dem, was Sie hören?
- **Import eines Faltungssatzes** und Filterwechsel während der Wiedergabe.
- **Upsampling nach DSD** an einem kompatiblen DAC: Stabilität, Pegel, keine Klicks.
- **Ketten-Voreinstellungen** in beiden Anwendungsmodi.
- **Ein Netzwerkausgang** (UPnP, AirPlay oder OAAT) mit aktiver Verarbeitung.
- **Aktualisierung von rc13.x** und die Schaltfläche „Authentifizierung deaktivieren“.

## Bekannte Probleme

- **DST-komprimierte SACDs**: Diese ISO-Abbilder werden noch nicht nativ abgespielt.
- **VU-Meter** bei einem weitergeleiteten MP3-, AAC- oder Ogg-Titel: Nach einem Sprung schalten sie sich bis zum Ende des Titels ab. Der Ton ist nicht betroffen.
- **Fernhören über CarPlay**: Die Korrektur des Relays ist auf die rc15 verschoben.
- **Server ohne Bildschirm, Authentifizierung aus**: Um das verknüpfte mozaiklabs.fr-Konto zu wechseln, die Authentifizierung aktivieren oder einen SSH-Tunnel zu localhost:8888 nutzen.
