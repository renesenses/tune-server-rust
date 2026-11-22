Tune 1.0.0-rc12, den tolvte utgivelseskandidaten av Tune 1.0. Flere visninger kommer i en ny utforming av Levente, designer fra Tune-fellesskapet.

## Nyheter

- **Nytt grensesnitt**: Tune Circle redesignet (Utforsk, Meldinger, Delte spillelister, Min krets); albumside med beskrivelse, medvirkende, detaljer og en fane Versjoner med katalognummeret; artistside med stort portrett, tall og faner; Innstillinger i en mer lesbar kolonne; Serverstatus og lyttedashbordet, med valg av periode.
- **Hjem**: en ny widget «Lytteoversikt (ny visning)», som legges til i Hjem › Rediger, med et panel Soner; større fliser med år og sjanger.
- **Bibliotek**: fest filtrene du vil med knappen Filtre; klassisk musikk med Dirigent, Ensemble / orkester og Verk; antall kanaler og et filter Stereo; kart og filter etter artistens land; «Erstatt sti» for en musikkmappe; oppdagelse av falsk Hi-Res, avslått som standard.
- **Teknisk ark for et spor**: spektrogram og bølgeform per kanal, også for Qobuz- og TIDAL-sporene i biblioteket.
- **Radio når køen tar slutt**: når køen er ferdig, fortsetter Tune med lignende spor, merket «Radio»; kan stilles inn sone for sone.
- **Innsovningstimer**: musikken stopper på valgt tidspunkt; på Tune OS kan maskinen også slå seg av.
- **Spillelister**: antall avspillinger og siste avspilling på hver linje; et kryss Fjern og «Fjern fra denne spillelisten»; fjern et spor fra din egen Qobuz-spilleliste.
- **Radio**: filtrene Tapsfri, Høy kvalitet og Hi-Res på én linje; en stasjon fra katalogen kan redigeres bare for deg; en radio kan legges til i en spilleliste.
- **Avspilling**: innebygd DSD på den lokale Linux-utgangen (ALSA); fast PCM-frekvens per sone; eksklusiv modus stilt inn sone for sone; slettede soner kan gjenopprettes; ekte AccurateRip-verifisering av CD-er.
- **DLNA**: sammenhengende strøm mellom sporene for renderere som skjøter to spor dårlig, avslått som standard; en gruppes hovedvolum kan stilles fra hvilket som helst medlem.
- **Strømming**: velg land for TIDAL-katalogen; Qobuz-nyheter er de samme som i Qobuz sin egen spiller; Spotify får en tilkoblingsveiviser, og Spotify Connect spiller i den valgte sonen.
- **Signalvei**: et klikk på et behandlingstrinn åpner innstillingene for det; gruppert avspilling viser hver utgang.
- **Innstillinger og grensesnitt**: søk etter innstilling, tekststørrelse, fremdriftslinje som bølgeform, ett enkelt søkefelt i alle visninger, taleinndata i assistenten.

## Forbedringer (improvements)

- **Hastighet**: smarte samlinger og alfabetiske hopp er mye raskere i et stort bibliotek; store lister bremser ikke lenger serveren.
- **Minne**: Tune gir tilbake frigjort minne under lange lytteøkter; minnet til omkodingen er begrenset.
- **Raskere lydstart**: YouTube starter raskere; TIDAL venter ikke lenger på at hele sporet er lastet ned på en lokal utgang eller OAAT-utgang.
- **Statistikk**: én enkelt sum av spor og album i alle visninger, én oppføring per kodek, søk i hele rangeringen.
- **Tune Circle**: emojier i Meldinger; når lyttingen hos en kontakt er stengt, leter Tune etter det samme sporet i dine egne kilder.
- **Mobilapper**: CarPlay spiller direkte på telefonen, med Samlinger og en fane Radio; Android får TIDAL-katalogen, historikk for radio og podkaster, og en widget Favorittradioer.
- **Windows**: et banner varsler når brannmuren filtrerer Tune; et WASAPI- eller ASIO-merke på lokale soner.
- **Docker**: veiviseren for SMB-deling fungerer i bildet; CD-stasjonen kan brukes i containeren.
- **Oversettelser**: riktige flertallsformer på alle 15 språk.
- **Avhengigheter** oppdatert, blant annet den innebygde SQLite 3.54.

## Feilrettinger (bugfikser)

- **Webgrensesnitt**: ingen svart skjerm lenger etter en oppdatering av grensesnittet.
- **Demping**: en dempet sone forblir stille når et nytt spor starter; en alarm eller en volumgrense opphever ikke lenger dempingen.
- **Avspilling**: en skadet fil fryser ikke lenger sonen, Tune går videre; en ferdigspilt podkastepisode åpnes ikke lenger på nytt ved slutten.
- **AirPlay 2**: avspillingen fortsetter der den stoppet etter en pause, og starter på sonens volum, aldri på full styrke.
- **DLNA og grupper**: avspillingen fortsetter etter en feil 701; neste spor klargjøres på forhånd; en enhet som ikke svarer, blokkerer ikke lenger andre soner; Sonos får navnet på modellen sin.
- **HQPlayer, Devialet og Diretta**: posisjon og status følges, automatisk gjenoppkobling; Devialet-forsterkeren svarer igjen; ingen falsk volumglidebryter lenger på Diretta.
- **OAAT**: innebygd DSD vises og er bit-perfect; DSD1024 gjenkjennes, med reserve til PCM på en begrenset mottaker.
- **Radio**: ingen dupliserte stasjoner lenger; en radio som avvises i ditt land, går over til sin neste strøm.
- **Spotify, Qobuz og TIDAL**: Spotify-spillelister og -favoritter følger det nye API-et; et avslag fra Qobuz vises ikke lenger som feil påloggingsopplysninger; forhåndslastingen i TIDAL forsinker ikke lenger avspillingen.
- **Bibliotek**: avspillinger telles per spor; en bindestrek skaper ikke lenger en falsk artist; platene i en boks gjenkjennes bedre; et spor uten år får albumets år.
- **Windows**: en manglende DAC sender ikke lenger lyden til PC-ens høyttalere; mappeovervåkingen dekker også nettverksressurser.
- **PostgreSQL 18**: UPnP-favoritter virker igjen.

## Sikkerhet

- Å administrere Tune fra en annen maskin i nettverket krever nå pålogging (Tune OS: frem til det nye bildet).
- Webhooks kan ikke lenger rette seg mot selve Tune-maskinen.
- Konfigurasjonen av Home Assistant og Hue krever administratorrettigheter eller selve Tune-maskinen; Home Assistant installert på samme maskin som Tune støttes ikke lenger.

## Kjente problemer

- **DSD via DoP på den lokale utgangen**: det kan spille på full styrke etter dempet PCM, uten å ta hensyn til volumet. Hold forsterkerens volum lavt når du går fra et PCM-spor til et DSD-spor via DoP (neste versjon).
- **Integritetskontroll**: mange FLAC-filer kan feilaktig bli meldt som ødelagte; ikke slett noen fil bare på grunnlag av denne vurderingen.
- **Windows ASIO styrt av en annen server**: ingen lyd etter Pause og deretter Spill; hopp til neste spor.
- **Etter oppdateringen**: hvis grensesnittet ikke endres, tving frem en ny innlasting (Cmd+Shift+R eller Ctrl+Shift+R). Første oppstart tar lengre tid mens to indekser opprettes.
