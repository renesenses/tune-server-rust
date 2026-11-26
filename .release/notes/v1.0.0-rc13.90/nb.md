Tune 1.0.0-rc13.90: **forhåndsversjon (pre-release) av rc14**, kun for testere. Dette er ingen publisert versjon: den automatiske oppdateringen tilbyr den ikke. Takk til alle som tester den.

## Nyheter

- **TESS, den 64-biters behandlingskjeden**: all behandling skjer i 64-biters flyttall hele veien, med én eneste kvantisering helt til slutt.
- **Samme vei for alle utganger**: lokal utgang, UPnP/DLNA, AirPlay, Devialet AIR og OAAT går gjennom TESS; en utgang som ikke kan ta imot en behandling, sier det tydelig.
- **Parametrisk equalizer** med frie bånd (frekvens, forsterkning, Q), ved siden av den grafiske equalizeren og hodetelefon-equalizeren (AutoEq-profiler).
- **Konvolusjon**: filtre som .wav, .zip-arkiv eller Convolver-konfigurasjoner (.cfg), ett filter per samplingsfrekvens, bytte uten avbrudd og oppgitt latens.
- **Romkorrigering ved måling**: et FIR-filter med blandet fase beregnet fra flere lytteposisjoner.
- **Oppsampling til DSD** (DSD64, DSD128, DSD256) med en sigma-delta-modulator.
- **Kjedeforhåndsvalg**: lagre en hel kjede og hent den fram, med en gang eller ved neste avspilling.
- **Signalvei**: fra avspillingslinjen, trinn for trinn, hva som faktisk brukes og hvorfor et trinn ikke gjør det.
- **Innebygde tillegg** i kjeden: aktivering under drift, 64-biters format, rekkefølge og antall kanaler respekteres.
- **Grensesnitt på 20 språk**: nederlandsk, tradisjonell kinesisk, russisk og ukrainsk kommer til.
- **Autentisering**: en knapp «Deaktiver autentisering» under Innstillinger › System › Autentisering.

## Forbedringer (improvements)

- **SACD-ISO**: innebygd DSD sendes til nettverksspillere som virtuell DSF; DSD overført via DoP melder seg som DSD.
- **Fjernlytting**: på 5G sendes første spor som FLAC.
- **Fraværende enheter**: glemmes automatisk etter 30 dager (justerbart), uten noen gang å slette en sone.
- **Cover+**: bedre omslag, også for nettradio (ICY-metadata).
- **Gruppeavspilling**: justerbar forsinkelse per sone; «Forrige» i den ledende sonen starter også de følgende sonene på nytt.
- **Bibliotek og strømming**: bakgrunnsgjennomganger blokkerer ikke lenger grensesnittet, alfabetisk albumsortering beholdes, lignende artister fra en tjenestes artistside.
- **Avvisningsmeldinger** er kortere og viser til riktig innstilling.

## Feilrettinger (bugfikser)

- **Rettingene i rc13.1 og rc13.2** er med: innlogging på administratorkontoen, kobling av mozaiklabs.fr-kontoen fra det lokale nettverket, omslag med aktiv autentisering, oppdatering i Windows.
- **UPnP**: Tune indekserer ikke lenger seg selv; et indeksert spor spilles fra serverens nåværende adresse.
- **Avspilling**: 30 s forhåndslasting uten forkastede buffere; et hopp som kilden avviser, forskyver ikke lenger strømmen.
- **Chromecast** kobler seg til igjen; ny publisering i AirPlay avbryter ikke lenger avspillingen.
- **Podkaster**: utilgjengelige episoder meldes riktig.
- **Rapporter fra forumet**: historikk, Qobuz, Spotify, «Tilbake»-knappen, søk og iPad.

## Endret oppførsel

- **Parametrisk equalizer**: gratis fram til rc13, er nå en del av Tune Premium, som konvolusjon, romkorrigering, declick i sanntid og tillegg fra tredjeparter. Den grafiske equalizeren, hodetelefon-equalizeren og crossfeed forblir gratis.

## Sjekk hjemme hos deg

- **En komplett TESS-kjede** (equalizer, crossfeed, konvolusjon eller romkorrigering): stemmer signalveien med det du hører?
- **Import av et konvolusjonssett** og filterbytte under avspilling.
- **Oppsampling til DSD** på en kompatibel DAC: stabilitet, nivå, ingen klikk.
- **Kjedeforhåndsvalg**, i begge bruksmodusene.
- **En nettverksutgang** (UPnP, AirPlay eller OAAT) med aktiv behandling.
- **Oppdatering fra rc13.x** og knappen «Deaktiver autentisering».

## Kjente problemer

- **DST-komprimerte SACD-er**: disse ISO-bildene spilles ennå ikke av direkte.
- **VU-målere** på et videresendt MP3-, AAC- eller Ogg-spor: etter et hopp slukkes de til sporet er slutt. Lyden påvirkes ikke.
- **Fjernlytting i CarPlay**: rettingen av reléet er utsatt til rc15.
- **Server uten skjerm med autentisering av**: for å bytte koblet mozaiklabs.fr-konto, aktiver autentisering eller bruk en SSH-tunnel til localhost:8888.
