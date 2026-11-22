Tune 1.0.0-rc11, den elfte versionskandidaten av Tune 1.0.

## Nyheter

- **Rumskorrigering**: ett tillägg, avstängt som standard, mäter rummet med mikrofon (svep, impulssvar, RT60) eller simulerar dess basmoder utan mikrofon och föreslår sedan ett mål och en automatisk equalizer som bara dämpar, med en profil per zon; utan Premium tonas faltningen med impulssvar ut.
- **Föräldrakontroll**: regler per profil i Inställningar › Profiler, döljning av explicit innehåll, tidsfönster och lyssningsbudget.
- **Uppsampling**: ett Premium-tillägg för högkvalitativ uppsampling kopplas in i den lokala utgången, med inställning per zon.
- **Avancerad fråga**: fråga biblioteket med ett uttryck, samma som definierar de smarta samlingarna.
- **En sida per album**: ett albums versioner grupperas och väljs på albumsidan; nya filter för DSD-kvalitet och flerkanal samt antalet kanaler visas.
- **Anteckningar och statistik**: personliga anteckningar i Markdown och bibliotekets statistik per fördelning.
- **Katalog och spektrogram**: exportera biblioteket som en utskrivbar HTML-katalog med aktuella filter; visa ett spårs spektrogram från dess meny; en knapp stoppar DR-analysen mellan två spår.
- **Världskartor**: en landskarta och ett landsfilter i Biblioteket; globen ritar gränser, kuster och stora floder och namnger landet när du pekar på det.
- **Poddar**: lägg till en podd med adressen till dess RSS-flöde.
- **TIDAL-mixar**: raden ”Dina mixar” i Streaming › TIDAL visar och spelar kontots personliga mixar.
- **Equalizer-import**: importera en equalizerfil (AutoEq, Equalizer APO, OPRA) med förhandsvisning av banden innan den tillämpas.
- **Biografier**: länkar till Tune-artister och -album och till källan; biografier och albumanteckningar kompletteras vid behov från Qobuz, utan att skriva över något och med möjlighet att ångra.
- **Spellistor**: ersätt ett otillgängligt tjänstespår på plats och reparera spellistan.
- **Tune Circle**: skriv till en hel cirkel i Meddelanden; allmänna cirklar och trådens samtycke går via reläet.
- **Fysisk samling**: omslag för varje skiva, sökning i streamingtjänsterna, bläddring efter mappar, genrer och stilar.

## Förbättringar

- **Radio**: filter på lägsta strömkvalitet i ”Fler filter”, ”Mina radiostationer” skilt från ”Senast spelade”, standardströmmen alltid spelbar i lyssningslandet.
- **Tune tested**: Tune föreslår att en enhet läggs till efter en timmes lyssning utan fel, från en särskild knapp; en enhet som redan finns i katalogen lägger till ett hushåll.
- **Bibliotek**: inställningen ”Bibliotek utvidgat med tjänsternas favoriter”, på som standard; snabbare sökning på ordbörjan; rutnätssökningen kräver alla ord; period Från / Till på årstidslinjen; mjukare rullning av spår.
- **Spårlistor**: kolumnernas ordning kan ändras och varje vy minns den; en spellistas vy får omslag, miniatyrstorlek och kompakt huvud; källmärken i Spellisthanteraren.
- **Start och återuppta**: välj panelerna i lyssningsöversikten; ”Fortsätt lyssna” visar en underrubrik per slag, återupptar en podd med titel och omslag (att återuppta från sparad position i en DLNA-zon kommer i nästa version), och utan dubbletter.
- **Historik**: en lyssning räknas först efter 30 s eller halva spåret; ”Spela” efter Stopp behåller pågående album eller spellista; poddavsnitt och radio bär stationens namn.
- **Lyssningsstatistik**: importerade lyssningar (Roon, Plex, m3u) räknas bland de mest spelade; smarta listor och Historiken räknar som Biblioteket; en Qobuz-lyssning räknas inte längre för det lokala albumet med samma namn.
- **Crosstalk och equalizer**: kurva för separation / färgning och högtalarrespons i inställningsvyn, målseparation från 1 dB; parametrisk graf låst som standard (Redigera / Klar) och en färg per kanal.
- **Tillägg**: en panel ”Tilläggens status” för administratören; ett katalogkort och dess lokala tillägg är nu ett enda tillägg.
- **Aviseringar**: fel finns kvar, pausas när du pekar på dem och grupperas; de sista hårdkodade felmeddelandena är översatta; undantagets råtext viks ihop under ”Detaljer”.
- **Bakgrundsanalyser**: förloppet visas, prioriteten är låg och standardvärdet försiktigt efter skanningen; berikningen fortsätter där den slutade och försöker igen efter ett 503; ”Kör genomgång” för skivbolagens logotyper fungerar även vid paus.
- **Support och fjärråtkomst**: felrapporten bifogar loggar; länken, QR-koden och kopieringsknappen är tillbaka i Inställningar › Fjärråtkomst.
- **Uppspelning**: skälen till omkodning namnges och FLAC levereras oförändrad när inget gäller; Nästa och Föregående hoppar över spår som tjänsten nekar.

## Rättningar

- **AirPlay 2**: knappen ”Pair” parar via AirPlay 2-komponenten eller anger orsaken; väntan på PIN-koden blockerar inte längre uppspelningen och att stänga fönstret avbryter den; sessionen öppnas på nytt efter en paus; positionen går framåt och en paus loopar inte längre; komponenten finns för 64-bitars Raspberry Pi.
- **DLNA och OpenHome**: gapless DSD-spår, snabbare ”nästa”, en paus gjord på enheten respekteras, maskinens egen utgång visas inte längre dubbelt, OpenHome-volym utan väntan, listan som Kazoo laddat bevaras, snabbare Play på TIDAL CD.
- **Volym**: en ny lokal utgång startar inte längre på full nivå; när ljudavstängningen hävs går reglaget tillbaka till föregående volym; varje reglerläge ger en egen nivå; WASAPI- och ASIO-volym hörs direkt; Tune följer enheten under säkerhetstaket.
- **Windows**: genvägen öppnas utan konsolfönster; en DoP som utgången inte kan bära skickas som PCM; nätverksupptäckten passerar brandväggen; en oläsbar temperatur bromsar inte längre analyserna; uppdateringen startar sitt växlingsskript vid avslut.
- **Uppdateringar och licens**: på Unix valideras den nya binären och webbgränssnittet innan de gamla ersätts, och ett fel anges tydligt; att ta bort licensen frigör även maskinen på webbplatsen.
- **Bibliotek**: foton och videor från en UPnP-server kommer inte längre in; en FLAC- + CUE-box blir ett enda album; DSD- och CD-utgåvor förblir två album; ett handgjort samlingsalbum splittras inte längre; en raderad spellista återskapas inte vid skanning; en omläsning utan artisttagg flyttar inte längre albumet till ”Unknown Artist”; att lägga till en mapp skannar bara den mappen.
- **Omslag och analyser**: som sista utväg den största bilden i mappen; analys av falsk Hi-Res över hela spårets längd; gapless MP3; integritetskontrollen förklarar inte längre hela FLAC-filer som avkortade.
- **Streamingtjänster**: Spotify-sökningen ger resultat, ett avslag är inte längre tyst och favoriterna går förbi 50 spår; TIDAL › Favoriter fryser inte längre Tune; ett Qobuz-konto utan favoriter ger snabbt en tom lista; ett konverterat spår startar i Safari.
- **Nätverk och lagring**: upptäckten återupptas när nätverket dyker upp efter Tune; en SMB-montering som misslyckas anger varför, även under Docker; säkerhetskopian lyckas på PostgreSQL; CD-extraktionen avvisar en full eller icke skrivbar destination; Tune lämnar Squeezebox-portarna till en Lyrion-server på samma maskin.
- **Vyer**: Konserter öppnas utan licens; ”Avgruppera alla” visas igen; Spelas nu följer albumbyte; en utgången streamingsession behåller fliken ”Återanslut”; Nytt alarm förblir synligt.
- **Raspberry Pi 5**: fläkten i Active Cooler snurrar igen (ny Tune OS-avbildning).
- **Säkerhet**: härdningsrättelser; på en server utan autentisering nekas export av konfigurationen med hemligheter i klartext (använd säkerhetskopian krypterad med en lösenfras).

## Kända problem

- **Ljudavstängning i en DLNA-zon**: en zon med avstängt ljud kan starta om med ljud när ett nytt spår startas. Sänk volymen i stället för att stänga av ljudet tills det är åtgärdat (nästa version).
- **Återuppta ett poddavsnitt**: i en DLNA-zon börjar ett återupptaget avsnitt om från början (nästa version).
- **Tune Circle**: cirklar som du är medlem i eller inbjuden till visas ännu inte i listan som de du har skapat (nästa version).
