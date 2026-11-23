Tune 1.0.0-rc13.2, Korrekturversion der rc13.1.

## Fehlerbehebungen

- Anmeldung: Das Passwort des Administratorkontos wird auf dem Server-Rechner und im lokalen Netzwerk auf dieselbe Weise geprüft; klarere Fehlermeldung (Benutzername: default).
- Wenn Sie sich nicht mehr anmelden können, verwenden Sie den Benutzernamen default und das zuletzt auf dem Server-Rechner eingegebene Passwort.
- Das mozaiklabs.fr-Konto kann jetzt von jedem Browser im lokalen Netzwerk (iPad, Telefon) verknüpft werden, solange noch kein Konto verknüpft ist.
- Vom lokalen Netzwerk aus kann auch dasselbe Konto erneut verbunden werden. Es durch ein anderes Konto zu ersetzen oder die Verknüpfung aufzuheben, bleibt dem Server-Rechner oder einem Administrator vorbehalten. Name und E-Mail-Adresse des verknüpften Kontos sind im lokalen Netzwerk sichtbar.
- Windows: Die Aktualisierung aus Tune heraus funktioniert wieder. Seit rc13 und rc13.1 musste man das Installationsprogramm verwenden.
- Tune OS in rc12, das sich nicht mehr aktualisiert: Das Werkzeug „Débloquer Tune“ (Windows, macOS) startet die Aktualisierung ohne jeden Befehl. Es wird separat heruntergeladen: https://github.com/renesenses/tune-server-rust/releases/tag/debloquer-tune-v1
