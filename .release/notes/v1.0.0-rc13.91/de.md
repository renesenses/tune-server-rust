Tune 1.0.0-rc13.91: **zweite Vorabversion (Pre-Release) der rc14**, nur für Tester. Dies ist keine veröffentlichte Version: Die automatische Aktualisierung bietet sie nicht an. Sie enthält den gesamten Inhalt der rc13.90. Danke an alle, die sie testen.

## Neuheiten

- **In Reihe kombinierbare Equalizer**: grafischer, parametrischer und Kopfhörer-Equalizer sowie die Raumkorrektur folgen in derselben TESS-Kette aufeinander; das Anwenden einer Raumkorrektur überschreibt die anderen Equalizer nicht mehr. Der Verstärkungsbereich des parametrischen Equalizers ist erweitert.
- **Vollständiges Bypass all**: Ein einziger Handgriff nimmt die gesamte TESS-Kette aus dem Signalweg und stellt sie unverändert wieder her.
- **Kopfhörer-Plugin** auf Basis der OPRA-Datenbank: Das Profil Ihres Kopfhörers wird erkannt, mit Vorverstärkung und Pegel.
- **Speaker Setup-Plugin** (Lautsprechereinstellung): Abstand, Verstärkung und Phase, Kanal für Kanal.
- **Upsampling nach DSD am lokalen Ausgang**: ein eigener Sigma-Delta-Modulator, und die DSD-Fähigkeiten des Ausgangs werden veröffentlicht.
- **Wählbares Upsampling-Ziel**: In TESS lässt sich auf einem Bildschirm festlegen, wohin das Upsampling erfolgt, nach PCM oder nach DSD.
- **Pegeltest des Mikrofons** vor der Raummessung.
- **Messung mit der nativen Abtastrate des Mikrofons**, von 44,1 bis 192 kHz.
- **TESS-Einstellungen im laufenden Betrieb**: Das Deaktivieren einer Stufe wirkt sofort, und die Vorverstärkung bleibt begrenzt.

## Verbesserungen

- **Raumkorrektur**: Die Kontrollmessung hört die Korrektur der Zone mit; nicht korrigierte Abweichungen werden erklärt, und die obere Grenze ist bis 1 kHz einstellbar.
- **Raumkorrektur**: Eingaben, letzte Messung und angewendete Korrektur sind bei der Rückkehr zum Bildschirm wieder da; Hinweise zur Mikrofonplatzierung und ein Kasten „Vor der Messung“ leiten durch die Messung.
- **Messung im PURE-Modus**: Die Lautstärke wird für die Dauer des Sweeps auf 100 % gesetzt und danach wiederhergestellt.
- **Signalweg**: Eine ausgeschaltete Stufe (Upsampling, Faltung) sagt, warum, in TESS und auf der Seite des Plugins; unter Windows im geteilten Modus zielt das Upsampling auf die Abtastrate des Mixers.
- **Mitwirkende**: das Instrument statt „Musiker“, der Autor getrennt vom Komponisten, ein Name pro Rolle.
- **Scan**: Der in den Tags vorhandene Barcode ergänzt das Album, wenn es keinen hat.
- **Serverstatus**: Eine Warnung erscheint, wenn der Cover-Cache nicht angelegt werden kann.

## Fehlerbehebungen

- **Kalibrierung**: Der Sweep bricht nicht mehr vor der angegebenen Länge ab und bleibt nicht mehr in der Warteschlange hängen; eine Zone ohne Ausgang wird klar abgelehnt.
- **Kalibrierung**: Eine Spitze am Anschlag blockiert die folgenden Bänder nicht mehr.
- **RT60**: „nicht messbar (Rauschen)“ statt eines falschen Werts; Fehlermeldungen bleiben mindestens 10 s lesbar.
- **Abtastraten-Warnung**: Sie erscheint nur noch außerhalb der akzeptierten Abtastraten.
- **Radio**: Der Senderwechsel wird nicht mehr vom vorherigen Sender überschrieben.
- **Cover**: Bei einer Neuinstallation wird der Cache im Datenordner angelegt.
- **TESS**: nur noch ein Hinweis „nächster Titel“, und die Hilfe zur Schaltfläche „Anwenden“ ist korrigiert.

## Verhaltensänderungen

- **Änderung der TESS-Kette während der Wiedergabe**: Eine deaktivierte Stufe wird sofort abgeschaltet; eine hinzugefügte oder wieder aktivierte Stufe ist ab der nächsten Wiedergabe zu hören.

## Bitte bei Ihnen prüfen

Diese Punkte wurden noch nicht auf echter Hardware getestet:

- **Das Upsampling nach DSD an einem echten DSD-DAC**: Stabilität, Pegel, keine Klicks.
- **Die Raummessung bei 44,1 kHz und bei 96 kHz** an einem echten Audio-Interface, mit dem Pegeltest des Mikrofons.
- **Unter Windows, mit WASAPI im geteilten Modus**: Wiedergabe und Upsampling.
- **Speaker Setup an einem Netzwerkausgang** (UPnP, AirPlay oder OAAT): Abstand, Verstärkung und Phase.

Und außerdem:

- **Mehrere Equalizer in Reihe** mit Bypass all: Entspricht der Signalweg dem, was Sie hören?
- **Das Kopfhörer-Plugin** mit Ihrem eigenen Kopfhörer.
- **Die Aktualisierung von der rc13.90.**

## Bekannte Probleme

- **DST-komprimierte SACDs**: Diese ISO-Abbilder werden noch nicht nativ abgespielt.
- **VU-Meter** bei einem weitergeleiteten MP3-, AAC- oder Ogg-Titel: Nach einem Sprung schalten sie sich bis zum Ende des Titels ab. Der Ton ist nicht betroffen.
- **Fernhören über CarPlay**: Die Korrektur des Relays ist auf die rc15 verschoben.
- **Server ohne Bildschirm, Authentifizierung aus**: Um das verknüpfte mozaiklabs.fr-Konto zu wechseln, die Authentifizierung aktivieren oder einen SSH-Tunnel zu localhost:8888 nutzen.
