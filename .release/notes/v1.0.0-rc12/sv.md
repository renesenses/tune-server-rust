Tune 1.0.0-rc12, den tolfte versionskandidaten av Tune 1.0. Flera vyer kommer i en ny design av Levente, designer från Tune-communityn.

## Nyheter

- **Nytt gränssnitt**: Tune Circle omgjort (Upptäck, Meddelanden, Delade spellistor, Min cirkel); albumsida med beskrivning, medverkande, detaljer och en flik Versioner med katalognumret; artistsida med stort porträtt, siffror och flikar; Inställningar i en mer lättläst kolumn; Serverstatus och lyssningsöversikten, med val av period.
- **Start**: en ny widget ”Lyssningsöversikt (ny layout)”, att lägga till i Start › Redigera, med en panel Zoner; större rutor med år och genre.
- **Bibliotek**: fäst de filter du vill med knappen Filter; klassisk musik med Dirigent, Ensemble / orkester och Verk; antal kanaler och ett filter Stereo; en karta och ett filter efter artistens land; ”Ersätt sökväg” för en musikmapp; identifiering av falsk Hi-Res, avstängd som standard.
- **Ett spårs tekniska blad**: spektrogram och vågform per kanal, även för Qobuz- och TIDAL-spåren i biblioteket.
- **Radio när kön tar slut**: när kön tar slut fortsätter Tune med liknande spår, märkta ”Radio”; inställbart zon för zon.
- **Insomningstimer**: musiken stannar vid vald tid; på Tune OS kan maskinen även stängas av.
- **Spellistor**: antal lyssningar och senaste lyssning på varje rad; ett kryss Ta bort och ”Ta bort från den här spellistan”; ta bort ett spår från din egen Qobuz-spellista.
- **Radio**: filtren Förlustfri, Hög kvalitet och Hi-Res på en rad; en station i katalogen kan redigeras bara för dig; en radio kan läggas till i en spellista.
- **Uppspelning**: inbyggt DSD på den lokala Linux-utgången (ALSA); fast PCM-frekvens per zon; exklusivt läge inställt zon för zon; borttagna zoner kan återställas; riktig AccurateRip-verifiering av cd-skivor.
- **DLNA**: kontinuerlig ström mellan spåren för renderare som skarvar två spår dåligt, avstängd som standard; en grupps huvudvolym kan ställas in från vilken medlem som helst.
- **Streaming**: välj land för TIDAL-katalogen; Qobuz nyutgivningar är desamma som i Qobuz egen spelare; Spotify får en anslutningsguide, och Spotify Connect spelar i den valda zonen.
- **Signalväg**: ett klick på ett bearbetningssteg öppnar dess inställningar; grupperad uppspelning visar varje utgång.
- **Inställningar och gränssnitt**: sökning per inställning, textstorlek, förloppsindikator som vågform, ett enda sökfält i alla vyer, röstinmatning i assistenten.

## Förbättringar

- **Hastighet**: smarta samlingar och alfabetiska hopp är mycket snabbare i ett stort bibliotek; stora listor saktar inte längre ned servern.
- **Minne**: Tune lämnar tillbaka frigjort minne under långa lyssningspass; minnet för omkodning är begränsat.
- **Snabbare ljudstart**: YouTube startar snabbare; TIDAL väntar inte längre på att hela spåret laddats ned på en lokal utgång eller OAAT-utgång.
- **Statistik**: en enda summa av spår och album i alla vyer, en post per codec, sökning i hela topplistan.
- **Tune Circle**: emojier i Meddelanden; när lyssningen hos en kontakt är stängd letar Tune efter samma spår i dina egna källor.
- **Mobilappar**: CarPlay spelar direkt på telefonen, med Samlingar och en flik Radio; Android får TIDAL-katalogen, historik för radio och poddar, och en widget Favoritradio.
- **Windows**: en banderoll varnar när brandväggen filtrerar Tune; ett WASAPI- eller ASIO-märke på lokala zoner.
- **Docker**: guiden för SMB-delning fungerar i avbildningen; cd-enheten kan användas i containern.
- **Översättningar**: korrekta pluralformer på alla 15 språk.
- **Beroenden** uppdaterade, bland annat det inbyggda SQLite 3.54.

## Rättningar

- **Webbgränssnitt**: ingen svart skärm längre efter en uppdatering av gränssnittet.
- **Ljudavstängning**: en zon med avstängt ljud förblir tyst när ett nytt spår startar; ett alarm eller en volymgräns slår inte längre på ljudet.
- **Uppspelning**: en skadad fil fryser inte längre zonen, Tune går vidare; ett färdigspelat poddavsnitt öppnas inte längre på nytt vid slutet.
- **AirPlay 2**: uppspelningen fortsätter där den stannade efter en paus, och startar på zonens volym, aldrig på full styrka.
- **DLNA och grupper**: uppspelningen fortsätter efter ett fel 701; nästa spår förbereds i förväg; en enhet som inte svarar blockerar inte längre andra zoner; Sonos får sin modells namn.
- **HQPlayer, Devialet och Diretta**: position och status följs, automatisk återanslutning; Devialet-förstärkaren svarar igen; inget falskt volymreglage längre på Diretta.
- **OAAT**: inbyggt DSD visas och är bit-perfect; DSD1024 känns igen, med reserv till PCM på en begränsad mottagare.
- **Radio**: inga dubbla stationer längre; en radio som nekas i ditt land går över till sin nästa ström.
- **Spotify, Qobuz och TIDAL**: Spotify-spellistor och -favoriter följer det nya API:et; ett avslag från Qobuz visas inte längre som felaktiga inloggningsuppgifter; förladdningen i TIDAL fördröjer inte längre uppspelningen.
- **Bibliotek**: lyssningar räknas per spår; ett bindestreck skapar inte längre en falsk artist; skivorna i en box känns igen bättre; ett spår utan år får sitt albums år.
- **Windows**: en frånkopplad DAC skickar inte längre ljudet till datorns högtalare; mappbevakningen omfattar även nätverksresurser.
- **PostgreSQL 18**: UPnP-favoriter fungerar igen.

## Säkerhet

- Att administrera Tune från en annan dator i nätverket kräver nu inloggning (Tune OS: fram till den nya avbildningen).
- Webhooks kan inte längre rikta sig mot själva Tune-maskinen.
- Konfigurationen av Home Assistant och Hue kräver administratörsrättigheter eller själva Tune-maskinen; Home Assistant installerat på samma maskin som Tune stöds inte längre.

## Kända problem

- **DSD via DoP på den lokala utgången**: det kan spelas på full nivå efter dämpad PCM, utan hänsyn till volymen. Håll förstärkarens volym låg när du går från ett PCM-spår till ett DSD-spår via DoP (nästa version).
- **Integritetskontroll**: många FLAC-filer kan felaktigt rapporteras som skadade; radera ingen fil enbart på grund av detta utlåtande.
- **Windows ASIO styrt av en annan server**: inget ljud efter Paus och sedan Spela; hoppa till nästa spår.
- **Efter uppdateringen**: om gränssnittet inte ändras, tvinga fram en omladdning (Cmd+Skift+R eller Ctrl+Skift+R). Första starten tar längre tid medan två index skapas.
