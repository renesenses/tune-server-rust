Tune 1.0.0-rc10, den tionde versionskandidaten av Tune 1.0.

## Nyheter

- **Min fysiska samling**: ett nytt tillägg jämför din Discogs-samling med ditt bibliotek och med streamingtjänsterna, i ett rutnät av omslag som cachas på servern. En egen post visas i sidofältet, direkt efter Tune Circle.
- **Mitt tema**: välj alla färger i gränssnittet, per profil. Ett tillägg kan också leverera ett tema, och du kan skicka ditt till en kontakt i Tune Circle, som tillämpar det med ett klick.
- **Crosstalk**: tillägget som minskar överhörningen mellan högtalare kommer in i tilläggskatalogen, med sin inställningsskärm; i samma zon stänger det av crossfeed och visar det.
- **Mappar**: spela upp eller lägg till i kön en mapp med alla dess undermappar, i ordning; bibliotekets katalogfilter går ner i undermapparna och erbjuder att återgå till hela biblioteket.
- **Radio**: ett fält ”Fler filter” i Mina radiostationer och i Katalogen gör det möjligt att söka efter kriterier; fyra nya genrer kommer till katalogen.
- **Radio vid uppspelning**: en stations strömmar sorteras efter kvalitet; Spelas nu visar stationens logotyp och en knapp ”Mina radiostationer”; lyssningslandet ställs in under Inställningar › Allmänt.
- **Skivbolag**: skivbolagens logotyper visas, delade av mozaiklabs.fr; serverstatus följer hur albumomslag och skivbolagslogotyper hämtas.
- **Volym**: reglaget är graderat i dB, komfort- och säkerhetsgränserna visas i färg, och en komfortgräns över säkerhetsgränsen avvisas.
- **Oxygen och metadata**: ändra ett fält i ett helt filtrerat resultat; undanta mappar från den automatiska metadataberikningen.
- **Tune Circle**: zooma in på ett område av jordklotet i Världsvyn; Tune Circle-spellistor har en egen etikett i spellistfiltren; en tjänsts spellista kan delas från dess sida.
- **Skrivbordswidget**: minifönstret använder webbgränssnittets minivy och får en sökning efter artister, Qobuz och TIDAL, med miniatyrer och tangentbordsnavigering.
- **AES67-utgång**: en experimentell utgång till AES67-kompatibla Dante- och Ravenna-ljudnätverk, avstängd som standard, finns under Inställningar › Experimentellt.

## Förbättringar

- **Den här webbläsaren**: nästa spår förbereds före slutet för mjukare övergångar, och serverns zon ”Den här webbläsaren” spelar utan förberedande inställning.
- **Fortsätt lyssna**: kortet anger vilket album, vilken spellista eller vilket avsnitt som återupptas, och en avslutad lyssning försvinner ur listan.
- **Historik och populära spår**: spåren i ett öppnat album eller en öppnad spellista går från nyast till äldst; en artists populära spår kompletteras med dem du lyssnar mest på, från alla källor.
- **Bibliotek**: filtren efter DR och efter originalår är betydligt snabbare i stora bibliotek; spårlistans kolumner kan ändras i bredd och varje skärm minns bredden; den akustiska analysen förklarar sin takt och visar sina framsteg.
- **Equalizer**: fri inmatning av frekvens och Q, som bekräftas när inmatningen är klar; borttagning av importerade förinställningar kräver bekräftelse; rumskorrigering och DAC-kalibrering tillämpar hela equalizerprofilen.
- **Spellistor**: import av .m3u, .m3u8 och .dpl finns i Spellisthanteraren och anger det verkliga antalet spår; Importera, Överför och Jämför säger varför de misslyckas.
- **Omslag**: inbäddade omslag läses från alla taggar, framsidan först; onlinesökningen följer efter omläsningen av filen; den rektangulära bilden för en Qobuz-spellista visas i sin helhet.
- **Meddelanden**: de sista felmeddelandena som fortfarande var på franska är översatta till gränssnittets språk; en AirPlay 2-högtalare som vägrar anslutningen anger att parkoppling saknas; ett applikations-ID som Qobuz avvisar visas inte längre som fel lösenord.
- **Licens**: skärmen anger att Premium kommer från ditt Mozaiklabs-konto, utan att begära någon nyckel.
- **Startsida**: ”Andra versioner” fastnar inte längre i väntan på streamingtjänsterna.
- **ASIO**: när DSD-stödet inte kan kontrolleras säger Tune det och förklarar vad utgången klarar.

## Rättningar

- **Uppspelning**: att växla mellan PURE och equalizer startar inte längre om en stoppad zon; att avmarkera ”Upprepa som standard” stänger verkligen av upprepningen för zonen; övergångar mellan kompatibla spår förblir obrutna.
- **OpenHome**: paus och stopp tar bort det förberedda nästa spåret från spelaren, och slutpositionen behålls under övergången till nästa spår.
- **Radio**: med okänt lyssningsland ersätts den valda strömmen inte längre av en ström på 96 kbit/s; en station som lagts till från konsolen behåller länken till sin sida; ett enstaka fel stoppar inte längre radiometadata; ”Senast spelade” visar rätt datum; Qobuz-spellistan som skapas från radiofavoriterna visas direkt.
- **Bibliotek och profiler**: filtren för favoriter och betyg följer den aktiva profilen; ”Mest spelade” räknar inte längre en lyssning flera gånger; berikningen ersätter inte längre en befintlig genre; UPnP-spår behåller år och genrer; ett MusicBrainz-ID som redan används tilldelas inte en andra gång.
- **Samlingar och spellistor**: en avvisad sparning rapporteras, en felaktig mapp tömmer inte längre listan, och alla sidor med spellistor läses.
- **Tillägg och konto**: att avinstallera ett aktivt inbyggt tillägg inaktiverar det varaktigt; den första kopplingen till ett Mozaiklabs-konto knyts till den enda profilen även utan e-postadress.
- **Övrigt**: spellistimporten vid skanning säger varför den inte importerar; ett fel vid läsning av inställningarna rapporteras i stället för att standardvärden används; när datorn går i viloläge rapporteras det inte längre som ett låsningsfel; det gamla gränssnittet överlever inte längre en serveruppdatering; VU-mätarfältet flödar inte längre över mellan 1280 och 1800 px; WASAPI-läget förblir synligt med Windows standardinställning; ljud komprimeras inte längre när det skickas.
- **Säkerhet**: härdningskorrigeringar.
