Tune 1.0.0-rc9, der neunte Release Candidate von Tune 1.0.

## Neuheiten

- **Tune Circle**: private Nachrichten zwischen Kontakten, mit Musikkarten, die sich mit einem Klick abspielen lassen, Löschen eigener Nachrichten und Melden. Nachrichten sind nicht Ende-zu-Ende-verschlüsselt und können nach einer Meldung von Mozaiklabs gelesen werden.
- **Tune Circle**: „Schreiben“ in der Kontaktliste öffnet direkt die Unterhaltung; Enter sendet; Darstellung für das Telefon angepasst.
- **Tune Circle**: Kreise, zu denen Sie eingeladen wurden, mit Playlist-Freigabe; ein Badge und eine Liste bündeln die Benachrichtigungen, je Typ einstellbar.
- **Tune Circle**: die Weltansicht zeigt die Community anonym, nach Land und nach Gebieten von etwa 110 km. Sie können Ihre Präsenz von der öffentlichen Karte entfernen.
- **Radio**: die globale Suche findet Ihre Sender und die des Verzeichnisses, mit mehreren Wörtern in beliebiger Reihenfolge. Eine Senderseite zeigt Genres, Website und Streams; Sie können Ihren bevorzugten Stream wählen.
- **Karten und Podcasts**: Sender nach Genre, Land und Netzwerk erkunden, mit Listenansicht; Podcasts aus mehreren Ländern durchsuchen.
- **Wiedergabe fortsetzen**: das Fortsetzen findet Album, Playlist oder Warteschlange wieder, auch bei Zufallswiedergabe. Bei Podcasts springt es einige Sekunden zurück, einstellbar von 0 bis 300 s.
- **Sammlungen**: Zufallssortierung in Sammlungen und Tags, mit „Neu mischen“; Lieblingsalben eines Dienstes lassen sich nach Jahr und Qualität sortieren.
- **Aktuelle Wiedergabe**: eine Schaltfläche „Teilen“ kopiert den Link zum aktuellen Titel.
- **Oxygen**: einen Tag auf ein ganzes gefiltertes Ergebnis anwenden oder entfernen, nach Vorschau und Bestätigung.
- **Darstellung**: neues Design „Mozaiklabs“; die Mozaiklabs-Anmeldeseite folgt Sprache und Design der Oberfläche.
- **Sprachen**: Norwegisch (Bokmål), Polnisch und Griechisch kommen zu Tune.
- **Android Auto**: Favoriten, Playlists, Radio, Warteschlange und Aktuelle Wiedergabe, mit den Steuerelementen der gewählten Zone (noch nicht im Auto getestet).

## Verbesserungen

- **Im Auto**: mit Android Auto wie mit CarPlay spielt Tune Remote auf „Dieses Telefon“; die Zone zu Hause bleibt unberührt.
- **Radio**: von Sendern veröffentlichte Cover werden angezeigt, mit dem Logo als Ersatz; das Verzeichnis aktualisiert sich ohne Neustart; die Stream-Auswahl beachtet Länderbeschränkungen.
- **TIDAL**: die Suche nach einer Hi-Res-Version geht weiter, wenn die erste Antwort nur CD-Qualität liefert, im Rahmen Ihres Abonnements.
- **HQPlayer**: Titelwechsel sind zuverlässiger, und ein Neustart, der nicht anläuft, wird gemeldet.
- **Devialet**: Spulen in einem Qobuz-Hi-Res-Titel setzt ohne Tonaussetzer fort; bei Devialet AIR wird die am Gerät gemessene Lautstärke getrennt angezeigt.
- **Lautstärke**: Lautstärke und Stummschaltung beachten die Fähigkeiten jeder Ausgabe; der KI-Assistent erhöht die Lautstärke nur in kleinen Schritten.
- **Ausgaben**: ein Gerät mit AirPlay und UPnP/DLNA erscheint in beiden Formen; ASIO und CoreAudio folgen dem Kanal-Zuordnungsraster genauer.
- **Bibliothek**: kompakte Geräteliste; der Ordnerfilter gilt auch für neu Hinzugefügtes; der Verlauf fasst Wiedergaben desselben Senders zusammen.
- **Fernhören**: der kontobasierte Zugang lässt sich ohne QR-Code einrichten, und mit dem Konto verknüpfte Geräte können entfernt werden.

## Fehlerbehebungen

- **Wiedergabe**: eine veraltete Positionsmeldung beendet einen Titel nach dem Spulen nicht mehr; sehr kurze WAV-Dateien unterbrechen den Ablauf nicht mehr; die Wiedergabe setzt nach einer vorübergehend abgelehnten Abtastratenänderung fort.
- **DLNA und Zonen**: DLNA-Player erhalten das angekündigte Mehrkanal-FLAC; ein Gerätewechsel in einer spielenden Zone stoppt das vorherige Gerät; eine abgelehnte Übertragung lässt die ursprüngliche Wiedergabe intakt.
- **TIDAL**: Startfehler durch bestimmte Audio-Links sind behoben.
- **Sonstiges**: CD-Rippen nach Stopp, Wecker in der richtigen Reihenfolge, neue Playlists sofort sichtbar, Update-Banner verschwindet nach der Installation, ListenBrainz- und Last.fm-Schlüssel mit PostgreSQL, Sicherungen ohne HTTP-Caches.
- **Sicherheit**: Korrektur am Relais für das Fernhören; Details folgen in einem separaten Sicherheitshinweis.

Spotify wird ohne nachgewiesene echte Wiedergabe ausgeliefert: Sagen Sie uns im Forum, ob die Wiedergabe mit Ihrem Konto funktioniert.
