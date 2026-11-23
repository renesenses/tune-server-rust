Tune 1.0.0-rc13.2, en rettelsesversjon av rc13.1.

## Feilrettinger (bugfikser)

- Pålogging: passordet til administratorkontoen kontrolleres på samme måte på servermaskinen og på det lokale nettverket; tydeligere feilmelding (brukernavn: default).
- Hvis du ikke lenger kan logge inn, bruk brukernavnet default og det siste passordet som ble skrevet inn på servermaskinen.
- mozaiklabs.fr-kontoen kan nå kobles til fra hvilken som helst nettleser på det lokale nettverket (iPad, telefon) når ingen konto er koblet til ennå.
- Fra det lokale nettverket kan du også koble til den samme kontoen på nytt. Å erstatte den med en annen konto, eller å koble den fra, er fortsatt forbeholdt servermaskinen eller en administrator. Navnet og e-postadressen til den tilkoblede kontoen er synlige fra det lokale nettverket.
- Windows: oppdatering fra Tune fungerer igjen. Siden rc13 og rc13.1 måtte man bruke installasjonsprogrammet.
- Tune OS-servere som står fast på rc12 og ikke lenger oppdateres: det nye verktøyet «Débloquer Tune» (Windows, Mac) løser det med noen få klikk, uten kommandoer. Hvis databasen er kopiert fra en annen datamaskin og ikke har kontoen default, spør det etter brukernavn og passord for administratorkontoen din. Last ned: https://github.com/renesenses/tune-server-rust/releases/tag/debloquer-tune-v1
