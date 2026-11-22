Tune 1.0.0-rc13, den trettonde releasekandidaten för Tune 1.0. Tack till Gros Bidon, r5yk5, Tades, cpadiou77, Pierre, Daniel LEVY, Lio, sashasha, ajm, Jean Valjean, Semakawa, Jon Rotaetxe, hifi25nl, aqs, Christopher J, FabienM, Begastor och Régis för deras rapporter och tester, till alec_eiffel för Smart Upsampler, till cometdom för Qobuz och till Levente för designen.

## Nyheter

- **Uppsamplare**: Smart Upsampler, utvecklad av Rémi (alec_eiffel), spelar på samma nivå som utan den, och en ändrad inställning hörs direkt.
- **Uppspelning och utgångar**: Sendspin och Devialet AIR får det bearbetade ljudet (equalizer, tillägg, uppsamplare). Insomningstimern kan stänga av datorn; skrivbordets mediekontroller på Linux.
- **Crossfeed**: gratis, med sina inställningar och förinställningar.
- **Faltning**: en impulssvarsfil per samplingsfrekvens.
- **DSD**: DSD från en UPnP-server spelas nativt på krävande nätverksspelare, och två DSD-spår följer varandra utan glapp.
- **Bibliotek**: dina personliga taggar blir Tune-taggar; flera album kan väljas för en gemensam åtgärd; filter för instrument.
- **Förändrad källa** ersätter ”Tveksam Hi-Res”, för alla format, med den uppmätta brytfrekvensen.
- **Vågform** finare, även för CUE-, Qobuz- och TIDAL-spår (som tillval).
- **Kö**: album kan fällas ihop och flyttas med en enda gest.
- **Start**: 5 spellistor ”Eftersom du gillar…” och widgetarna Mest spelade artister, Ofta spelade, Lyssna senare och Fortsätt lyssna.
- **Tjänster**: TIDAL:s sida ”För dig”; 200 Qobuz-nyheter per genre.
- **Tune Circle**: medlemmarna i en krets kan se varandra om de godkänner det, och vyn Världen visar alla kretsar.
- **Webbgränssnitt**: turkiska, autentisering som kan aktiveras från gränssnittet och historik per spår eller per lyssning.
- **Tune Client (skrivbordsapp)**: hittar själv servrarna i nätverket och styr den lokala servern.
- **Windows**: start av Tune tillsammans med Windows, som installationsprogrammet erbjuder.
- **iOS-app**: CarPlay över 5G mer komplett, sidofältet tillbaka på iPad.

## Förbättringar

- **Övergångar**: ReplayGain ”Auto” behåller albumförstärkningen mellan två spår från samma album; AAC, Opus och CUE-spår spelas utan glapp.
- **Uppspelning och utgångar**: samma volymkurva i AirPlay 1 och 2; en radiokanal startar snabbare på en nätverksspelare.
- **Windows**: i WASAPI:s exklusiva läge byter Tune till den andra frekvensfamiljen i stället för att stoppa spåret.
- **Devialet AIR**: volym i steg om 1 dB, upp till 192 kHz.
- **DSD**: identifieringen av nativ DSD är försiktigare och omprövas.
- **Stora bibliotek**: Mappar öppnas utan väntan, och bakgrundsuppgifter blockerar inte längre resten.
- **Tjänster**: Tunes egen Google-inloggningsklient för YouTube; Spotify och TIDAL stabilare under uppspelning.
- **Webbgränssnitt**: mer exakt parametrisk equalizer och bekräftelse innan något raderas.
- **iOS-app**: låsskärmen styr zonen även när den inte spelar; utan DAC styr reglaget volymen i iOS.
- **Uppdatering och diagnostik**: uppdateringen ersätter även program som levereras med Tune; felrapporter döljer namnen.

## Rättningar

- **Uppspelning och utgångar**: återupptagning fungerar efter en paus som hade släppt DAC:n; ”Upprepa alla” förväxlar inte längre ett kort stopp med slutet på ett spår; på Windows hittas en DAC som slagits på efter Tune.
- **Zoner**: en redan kopplad enhet namnger sin zon; med OAAT öppnar en återupptagning sessionen igen i stället för att förbli tyst.
- **DLNA, UPnP och OpenHome**: inget oönskat Stopp på Musicbook Source, stabil ljudavstängning, Linn Klimax DS/2 känns igen.
- **AirPlay**: AirPlay 1-utgången startar inte längre på max efter en omstart.
- **Signalväg**: den redovisar inte längre bearbetning som inte sker.
- **Bibliotek**: DSD- och cd-utgåvorna av samma album slås inte längre ihop; skrivningen av DR fryser inte längre servern.
- **Tjänster**: Qobuz- och TIDAL-favoriter med rätt längd; Spotify-avsnitten visas inte längre dubbelt.
- **Radio**: FIP-kanalerna får tillbaka sin logotyp.
- **Tillägg och crossfeed**: ett installerat tillägg verkar utan omstart; crossfeed är inte längre låst med OAAT eller i webbläsaren.
- **Webbgränssnitt**: rutnätet rullar ända till slutet; bannern ”servern svarar inte” försvinner så snart servern svarar.

## Säkerhet

- Flera säkerhetsförstärkningar.
- Loggar och felrapporter döljer nu de inloggningsuppgifter som vissa strömadresser innehåller.
- Ersätt sökvägen (Inställningar › musikmappar) kontrollerar nu mapparna och kan inte längre nås från en annan enhet utan autentisering.

## Ändrat beteende

- **Tune Circle**: med autentisering reserverat för administratörer; skrivskyddat vid fjärrlyssning.
- **YouTube**: konton som anslutits före denna version måste anslutas igen en gång.
- **Radio när kön tar slut**: åter avstängd som standard; markera den igen i zonen om du hade valt den.
- **Equalizer**: när equalizern stängs av behålls dess förförstärkning fram till ”Återställ nivån”.
- **Multiroom**: grupp gratis upp till 3 zoner; därutöver Premium. Stereopar förblir gratis.
- **CUE med INDEX 02**: spårens början rättas vid nästa skanning, historiken behålls.
- **Tune OS**: det tillfälliga lösenordet visas på maskinens skärm eller på startkortet, inte längre på inloggningssidan; fjärråtkomsten förblir stängd tills det har ändrats.
- **Profiler och historik**: med autentisering hanterar bara administratören de andra profilerna.
- **Ersätt sökvägen**: utan autentisering, från maskinen där Tune körs.

## Att kontrollera hemma

De här ändringarna har klarat de automatiska testerna utan prov på riktig hårdvara. Berätta vad du ser:

- **Sendspin och Devialet AIR** med en aktiv bearbetning.
- **Windows**: minispelaren, WASAPI:s exklusiva läge, start tillsammans med Windows.
- **AirPlay 1**: nivån vid start och mellan spåren.
- **DSD till en nätverksspelare** (Atoll ST200, Eversolo).
- **Linn Klimax DS/2** som OpenHome, **Tune Client på macOS** och **CarPlay över 5G**.

## Kända problem

- **DSD via DoP på den lokala utgången**: efter dämpad PCM kan det spelas på full nivå. Håll förstärkarens volym låg.
- **UPnP efter adressbyte**: ett spår kanske inte spelas; indexera om den servern.
- **Integritetskontroll**: några filer kan rapporteras felaktigt; radera inget enbart på grund av detta utlåtande.
- **Windows ASIO styrt av en annan server**: inget ljud efter Paus och sedan Spela; hoppa till nästa spår.
- **Efter uppdateringen**: om gränssnittet inte ändras, tvinga fram en omladdning (Cmd+Skift+R eller Ctrl+Skift+R).
