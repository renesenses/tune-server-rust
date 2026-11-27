Tune 1.0.0-rc14 inneholder alt innholdet i forhåndsversjonene rc13.90 og rc13.91; disse merknadene beskriver hva som er endret siden rc13.91. Takk til testerne: mange av punktene kommer fra tilbakemeldingene deres.

## Nyheter

- **TESS, forhåndsinnstillinger Lagre og Last inn på nytt**: så snart en innstilling avviker fra den valgte forhåndsinnstillingen, viser TESS «endret». **Lagre** overskriver den opprinnelige forhåndsinnstillingen, **Last inn på nytt** går tilbake til den. En forhåndsinnstilling som følger med Tune, som «Pure», blir ikke overskrevet: «Lagre som…» lager en kopi.
- **TESS, DSD-forhåndsinnstilling brukt med en gang**: en forhåndsinnstilling som endrer DSD-målet (for eksempel DSD128 → DSD256), lastet inn i modusen «bruk umiddelbart», starter sporet på nytt ved posisjonen sin i stedet for å vente på neste spor. Oppsampling er en Premium-funksjon.
- **Headroom**: Automatisk, eller for hånd fra −12 til 0 dB i trinn på 0,5 dB. Panelet viser verdien som faktisk brukes, eller sier hvorfor den ikke brukes.
- **Headroom ved oppsampling til DSD**: den manuelle headroomen brukes ikke lenger to ganger (−12 dB ga −24 dB; nå er det −15 dB, hvorav −3 dB for DSD-modulatoren, vist på en egen linje).
- **Rommåling med veiledning mot 75 dB(C)**: med en kalibrert mikrofon viser skjermen det målte nivået, avviket fra 75 dB(C) og en volumanvisning. Regelen er forsiktig, siden omregningen ennå ikke er kontrollert mot en kalibrator: Tune råder deg aldri til å gå over anslåtte 80 dB(C) og kutter signalet ved 90 dB SPL. Korreksjon målt med mikrofon er Premium; korreksjon beregnet uten mikrofon er fortsatt gratis.
- **Hørselsvern**: å fjerne avkrysningen for en equalizer, bytte til PURE eller Bypass all, eller fjerne en konvolusjon, en crossfeed eller en mononedmiksing får ikke lenger nivået til å stige brått: det går tilbake til det forrige nivået med en overtoning på omtrent et halvt sekund. Hvis du har skrudd opp volumet i mellomtiden, tennes indikatoren «nivå redusert», og «Gjenopprett nivå» frigjør overskuddet. Dette gjelder også nettverksutgangene (UPnP, OAAT).
- **En tro signalvei**:
  - på en nettverksutgang følger den strømmens faktiske rekkefølge, helt til spillerens volum;
  - trinnet for et impulsresponsfilter heter «Konvolusjon»;
  - filteret beholder navnet på den importerte .wav- eller .zip-filen;
  - etter en endring av DSD-målet viser den DSD-raten som faktisk spilles, og varsler den neste;
  - vinduet blir grønt: oransje er forbeholdt reelle varsler.
- **Konvolusjon**: panelet viser kurven til det innlastede filteret, kanal for kanal, og fasen på forespørsel.
- **Bypass all**: TESS-ikonet i avspillingslinjen dempes når TESS er forbigått, og TESS-siden viser «TESS forbigått».
- **Harmonisert TESS-side**: aksent i TESS-grønt, gjeldende sone ved siden av merket, panelet Headroom nedtonet på en nettverksutgang, og måling «Uten mikrofon (anslag)» eller «Med mikrofon (anbefalt)».
- **Nettverksutgang**: kortet Oppsampling sier at oppsampling ikke gjelder der.
- **Harmonisert grensesnitt**: innebygde skrifter, også uten nett; Bibliotek, Live Radio, Medieservere og Konserter følger den nye grafiske profilen.

## Feilrettinger (bugfikser)

- **Radio til en Sonos** (og enhver nettverksspiller med et tak på samplingsraten): en radiostasjon i 192 kHz leveres med den raten spilleren godtar. Før stoppet spilleren med en gang, om og om igjen.
- **768 kHz**: listen «Maks. samplingsrate» for en sone tilbyr 768 kHz og 1 536 kHz.
- **Bibliotek**: albumrutenettet fryser ikke lenger når du blar gjennom en stor samling.
- **Avspilling i nettleserfanen**: å spole i sporet flytter avspillingen på ordentlig, også fra en annen enhet.
- **Hjelp**: banneret om prioritert support vises ikke lenger for en Premium-konto.
- **Lisens**: utløpsdatoen leses riktig, og Innstillinger › Lisens sier hvorfor nivået er gratis.
- **Konvolusjon**: et filter som forsterker enkelte frekvenser, klipper ikke lenger.
- **Lange podkaster**: en episode der verten jevnlig bryter forbindelsen, blir ikke lenger avbrutt.
- **Debian-pakke i en LXC-container** (Proxmox): tjenesten starter selv uten private monteringsnavnerom.
- **Første oppstart**: veiviseren kaller ikke lenger tjenester uten konto «aktivert», og den lagrer den valgte musikkmappen. En tom «Mine radioer» åpner katalogen.

## Sikkerhet og kontoer

- **Fornyet økt**: med «Forbli pålogget» (ikke avkrysset som standard) fornyes nettleserøkten så lenge den brukes, opptil 90 dager. Ellers 24 t som før. Avatarmenyen tilbyr «Logg ut av denne serveren».
- **Gjenopprette administratortilgang**: kjør `tune-server --reset-admin` (eller `--reinitialiser-admin`) **på servermaskinen**. Engangskoden du får, gyldig i 15 minutter, skrives inn under «Glemt passordet?» på den maskinen; uten nettleser legger du til `--password-stdin`. Åpne økter lukkes. Installasjonsveiledningen (avsnitt 9) gir kommandoen for hvert system.
- **Profilen `admin`**: administratorprofilen som opprettes som standard, heter nå `admin`; et valgt navn beholdes, og `default` godtas fortsatt.
- **Administratorpassord fra første oppsett**: på en ny installasjon lar veiviseren deg opprette det.
- **Server uten autentisering**: administrasjon godtas fra det lokale nettverket, ikke lenger bare fra servermaskinen. **Advarsel**: uten autentisering kan enhetene på det lokale nettverket administrere serveren; slå på autentisering hvis nettverket ditt ikke er til å stole på. Innstillingen «Administrasjon bare fra denne maskinen» gjenoppretter den tidligere regelen.

## Fjernkontroller

- På en beskyttet server ber apper som ennå ikke er oppdatert, om passordet på nytt hver 24. time, som før.

## Sjekk hjemme hos deg

- **«Bypass all» i en forhåndsinnstilling**: en forhåndsinnstilling som endrer både Bypass all og DSD-målet, i modusen «bruk umiddelbart», starter sporet på nytt ved posisjonen sin. Bare kontrollert i testbenk: si fra om gjenopptakelsen er ren.
