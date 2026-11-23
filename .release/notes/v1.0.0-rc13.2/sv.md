Tune 1.0.0-rc13.2, en rättningsversion av rc13.1.

## Rättningar

- Inloggning: administratörskontots lösenord kontrolleras på samma sätt på serverdatorn och i det lokala nätverket; tydligare felmeddelande (användarnamn: default).
- Om du inte längre kan logga in, använd användarnamnet default och det senaste lösenordet som angavs på serverdatorn.
- mozaiklabs.fr-kontot kan nu kopplas från valfri webbläsare i det lokala nätverket (iPad, telefon) när inget konto är kopplat än.
- Från det lokala nätverket kan du också koppla samma konto igen. Att ersätta det med ett annat konto, eller koppla bort det, är fortfarande förbehållet servermaskinen eller en administratör. Namn och e-postadress för det kopplade kontot syns från det lokala nätverket.
- Windows: uppdatering inifrån Tune fungerar igen. Sedan rc13 och rc13.1 behövde man använda installationsprogrammet.
- Tune OS-servrar som har fastnat på rc12 och inte längre uppdateras: det nya verktyget ”Débloquer Tune” (Windows, Mac) löser det med några klick, utan kommandon. Hämta: https://github.com/renesenses/tune-server-rust/releases/tag/debloquer-tune-v1
