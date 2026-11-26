Tune 1.0.0-rc13.91: **andre forhåndsversjon (pre-release) av rc14**, kun for testere. Dette er ingen publisert versjon: den automatiske oppdateringen tilbyr den ikke. Den inneholder alt innholdet i rc13.90. Takk til alle som tester den.

## Nyheter

- **Equalizere som kan kombineres i serie**: grafisk equalizer, parametrisk equalizer, hodetelefon-equalizer og romkorrigering følger etter hverandre i samme TESS-kjede; å bruke en romkorrigering overskriver ikke lenger de andre equalizerne. Forsterkningsområdet til den parametriske equalizeren er utvidet.
- **Komplett Bypass all**: ett enkelt grep kobler hele TESS-kjeden ut, og gjenoppretter den nøyaktig som før.
- **Hodetelefon-tillegg** hentet fra OPRA-databasen: profilen til hodetelefonene dine gjenkjennes, med forforsterkning og nivå.
- **Speaker Setup-tillegg** (oppsett av høyttalere): avstand, forsterkning og fase, kanal for kanal.
- **Oppsampling til DSD på den lokale utgangen**: en egen sigma-delta-modulator, og utgangens DSD-egenskaper publiseres.
- **Valgfritt mål for oppsamplingen**: i TESS lar en skjerm deg velge hva det skal oppsamples til, PCM eller DSD.
- **Nivåtest av mikrofonen** før måling av rommet.
- **Måling på mikrofonens opprinnelige samplingsfrekvens**, fra 44,1 til 192 kHz.
- **TESS-innstillinger i farten**: å deaktivere et trinn virker med en gang, og forforsterkningen holdes innenfor grensene.

## Forbedringer (improvements)

- **Romkorrigering**: kontrollmålingen hører sonens korrigering; avvik som ikke korrigeres, forklares, og den øvre grensen kan justeres opp til 1 kHz.
- **Romkorrigering**: det du har fylt inn, den siste målingen og den aktive korrigeringen er der igjen når du kommer tilbake til skjermen; råd om plassering av mikrofonen og en boks «Før målingen» veileder målingen.
- **Måling i PURE-modus**: volumet tvinges til 100 % mens sveipet pågår, og settes deretter tilbake slik det var.
- **Signalvei**: et avslått trinn (oppsampling, konvolusjon) sier hvorfor, i TESS og på tilleggets side; i Windows i delt modus sikter oppsamplingen mot mikserens frekvens.
- **Medvirkende**: instrumentet i stedet for «Musiker», tekstforfatteren skilt fra komponisten, ett navn per rolle.
- **Skanning**: strekkoden i taggene fyller ut albumet hvis det mangler en.
- **Serverstatus**: en advarsel vises hvis hurtigbufferen for omslag ikke kan opprettes.

## Feilrettinger (bugfikser)

- **Kalibrering**: sveipet stopper ikke lenger før den oppgitte lengden og blir ikke lenger stående fast i køen; en sone uten utgang avvises tydelig.
- **Kalibrering**: en topp i taket blokkerer ikke lenger de neste båndene.
- **RT60**: «ikke målbar (støy)» i stedet for en feil verdi; feilmeldingene forblir lesbare i minst 10 s.
- **Frekvensadvarsel**: den vises nå bare utenfor de godtatte frekvensene.
- **Radio**: stasjonsbyttet overskrives ikke lenger av den forrige stasjonen.
- **Omslag**: på en ny installasjon opprettes hurtigbufferen under datamappen.
- **TESS**: bare ett varsel om «neste spor», og hjelpeteksten til «Bruk»-knappen er rettet.

## Endret oppførsel

- **Endring av TESS-kjeden under avspilling**: et deaktivert trinn kobles ut umiddelbart; et trinn som legges til eller aktiveres igjen, høres fra neste avspilling.

## Sjekk hjemme hos deg

Disse punktene er ennå ikke testet på ekte maskinvare:

- **Oppsampling til DSD på en ekte DSD-DAC**: stabilitet, nivå, ingen klikk.
- **Rommåling ved 44,1 kHz og 96 kHz** på et ekte lydkort, med nivåtesten av mikrofonen.
- **I Windows, med delt WASAPI**: avspilling og oppsampling.
- **Speaker Setup på en nettverksutgang** (UPnP, AirPlay eller OAAT): avstand, forsterkning og fase.

Og også:

- **Flere equalizere i serie** med Bypass all: stemmer signalveien med det du hører?
- **Hodetelefon-tillegget** med dine egne hodetelefoner.
- **Oppdatering fra rc13.90.**

## Kjente problemer

- **DST-komprimerte SACD-er**: disse ISO-bildene spilles ennå ikke av direkte.
- **VU-målere** på et videresendt MP3-, AAC- eller Ogg-spor: etter et hopp slukkes de til sporet er slutt. Lyden påvirkes ikke.
- **Fjernlytting i CarPlay**: rettingen av reléet er utsatt til rc15.
- **Server uten skjerm med autentisering av**: for å bytte koblet mozaiklabs.fr-konto, aktiver autentisering eller bruk en SSH-tunnel til localhost:8888.
