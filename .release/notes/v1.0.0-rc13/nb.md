Tune 1.0.0-rc13, den trettende utgivelseskandidaten for Tune 1.0. Takk til Gros Bidon, r5yk5, Tades, cpadiou77, Pierre, Daniel LEVY, Lio, sashasha, ajm, Jean Valjean, Semakawa, Jon Rotaetxe, hifi25nl, aqs, Christopher J, FabienM, Begastor og Régis for rapporter og tester, til alec_eiffel for Smart Upsampler, til cometdom for Qobuz og til Levente for designet.

## Nyheter

- **Oppsampler**: Smart Upsampler fra Rémi (alec_eiffel) spiller på samme nivå som uten den, og en endret innstilling høres med en gang.
- **Avspilling og utganger**: Sendspin og Devialet AIR mottar den behandlede lyden (equalizer, utvidelser, oppsampler). Innsovningstimeren kan slå av datamaskinen; på Linux vises Tune i skrivebordets mediekontroller.
- **Crossfeed**: gratis, med innstillingene og forhåndsinnstillingene sine.
- **Konvolusjon**: én impulsresponsfil per samplingsfrekvens.
- **DSD**: DSD fra en UPnP-server spilles nativt på krevende nettverksspillere, og to DSD-spor følger hverandre uten opphold.
- **Bibliotek**: dine personlige tagger blir Tune-tagger; flere album kan velges for en samlet handling; filter etter instrument.
- **Endret kilde** erstatter «Tvilsom Hi-Res», for alle formater, med den målte grensefrekvensen.
- **Bølgeform** finere, også for CUE-, Qobuz- og TIDAL-spor (som et valg).
- **Kø**: album kan felles sammen og flyttes med én bevegelse.
- **Hjem**: 5 spillelister «Fordi du liker…» og widgetene Mest spilte artister, Ofte spilt, Lytt senere og Fortsett å lytte.
- **Tjenester**: TIDAL-siden «For deg»; Qobuz viser 200 nyheter per sjanger.
- **Tune Circle**: medlemmene i en krets kan se hverandre hvis de godtar det, og visningen Verden viser alle Tune-kretser.
- **Nettgrensesnitt**: tyrkisk, autentisering som slås på fra grensesnittet, og historikk per spor eller per avspilling.
- **Tune Client (skrivebordsapp)**: finner selv Tune-serverne i nettverket og styrer den lokale serveren.
- **Windows**: installasjonsprogrammet tilbyr å starte Tune sammen med Windows.
- **iOS-app**: mer komplett CarPlay på 5G, sidefeltet er tilbake på iPad.

## Forbedringer (improvements)

- **Overganger**: ReplayGain «Auto» beholder albumforsterkningen mellom to spor fra samme album; AAC, Opus og CUE-spor spilles uten opphold.
- **Avspilling og utganger**: samme lydstyrkekurve i AirPlay 1 og 2; en radio starter raskere på en nettverksspiller.
- **Windows**: i WASAPI eksklusiv modus bytter Tune til den andre frekvensfamilien i stedet for å stoppe sporet.
- **Devialet AIR**: lydstyrke i trinn på 1 dB, opp til 192 kHz.
- **DSD**: gjenkjenningen av nativ DSD er mer forsiktig og vurderes på nytt.
- **Store biblioteker**: Mapper åpnes uten å vente, og bakgrunnsoppgaver blokkerer ikke lenger resten.
- **Tjenester**: Tunes egen Google-påloggingsklient for YouTube; Spotify og TIDAL er mer robuste under avspilling.
- **Nettgrensesnitt**: mer presis parametrisk equalizer og bekreftelse før sletting.
- **iOS-app**: låseskjermen styrer sonen selv når den ikke spiller; uten DAC styrer glidebryteren lydstyrken i iOS.
- **Oppdatering og diagnostikk**: oppdateringen erstatter også programmene som følger med Tune; feilrapporter skjuler navnene.

## Feilrettinger (bugfikser)

- **Avspilling og utganger**: gjenopptakelse fungerer når en pause hadde frigitt DAC-en; «Gjenta alle» forveksler ikke lenger et kort stopp med slutten på et spor; på Windows blir en DAC som slås på etter Tune, funnet.
- **Soner**: en enhet som allerede er koblet, navngir sonen sin; med OAAT åpner en gjenopptakelse økten på nytt i stedet for å forbli stille.
- **DLNA, UPnP og OpenHome**: ikke lenger unødvendig Stopp på Musicbook Source, stabil demping, Linn Klimax DS/2 gjenkjennes.
- **AirPlay**: AirPlay 1-utgangen starter ikke lenger på maks etter en omstart av Tune.
- **Signalvei**: den viser ikke lenger en behandling som ikke skjer.
- **Bibliotek**: DSD- og CD-utgavene av samme album slås ikke lenger sammen; skriving av DR fryser ikke lenger serveren.
- **Tjenester**: Qobuz- og TIDAL-favoritter med riktig varighet; Spotify-delene vises ikke lenger dobbelt.
- **Radioer**: FIP-kanalene får tilbake logoen sin.
- **Utvidelser og crossfeed**: en installert utvidelse virker uten omstart; crossfeed er ikke lenger låst med OAAT og i nettleseren.
- **Nettgrensesnitt**: rutenettet ruller helt til slutten; banneret «serveren svarer ikke» forsvinner så snart serveren svarer.

## Sikkerhet

- Flere sikkerhetsforsterkninger.
- Logger og feilrapporter skjuler nå påloggingsopplysningene som enkelte strømadresser inneholder.
- Erstatt stien (Innstillinger › musikkmapper) kontrollerer nå mappene og kan ikke lenger nås fra en annen enhet uten autentisering.

## Endret oppførsel

- **Tune Circle**: med autentisering forbeholdt administratorer; skrivebeskyttet fra ekstern lytting.
- **YouTube**: kontoer som er koblet til før denne versjonen, må kobles til på nytt én gang.
- **Radio når køen tar slutt**: igjen slått av som standard; kryss den av igjen i sonen hvis du hadde valgt den.
- **Equalizer**: når equalizeren slås av, forblir forforsterkeren i bruk fram til «Gjenopprett nivået».
- **Multiroom**: en gruppe er gratis opptil 3 soner; over det kreves Premium. Stereopar forblir gratis.
- **CUE med INDEX 02**: sporstartene rettes ved neste skanning, historikken beholdes.
- **Tune OS**: det midlertidige passordet vises på maskinens skjerm eller på oppstartskortet, ikke lenger på påloggingssiden; ekstern tilgang forblir stengt til det er endret.
- **Profiler og historikk**: med autentisering er det bare administratoren som styrer de andre profilene.
- **Erstatt stien**: uten autentisering, fra maskinen Tune kjører på.

## Sjekk hjemme hos deg

Disse endringene har bestått de automatiske testene uten å være prøvd på ekte maskinvare. Fortell oss hva du ser:

- **Sendspin og Devialet AIR** med en aktiv behandling.
- **Windows**: minispilleren, WASAPI eksklusiv modus, start sammen med Windows.
- **AirPlay 1**: nivået ved start og mellom sporene.
- **DSD til en nettverksspiller** (Atoll ST200, Eversolo).
- **Linn Klimax DS/2** som OpenHome, **Tune Client på macOS** og **CarPlay på 5G**.

## Kjente problemer

- **DSD via DoP på den lokale utgangen**: etter dempet PCM kan det spille på full styrke. Hold forsterkerens lydstyrke lav.
- **UPnP etter adresseendring**: et spor spilles kanskje ikke; indekser den serveren på nytt.
- **Integritetskontroll**: noen filer kan fortsatt bli feilaktig meldt; ikke slett noe bare på grunnlag av denne vurderingen.
- **Windows ASIO styrt av en annen server**: ingen lyd etter Pause og deretter Spill av; hopp til neste spor.
- **Etter oppdateringen**: hvis grensesnittet ikke endres, tving fram en ny innlasting (Cmd+Shift+R eller Ctrl+Shift+R).
