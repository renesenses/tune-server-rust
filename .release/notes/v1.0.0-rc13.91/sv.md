Tune 1.0.0-rc13.91: **andra förhandsversionen (pre-release) av rc14**, endast för testare. Det här är ingen publicerad version: den automatiska uppdateringen erbjuder den inte. Den innehåller allt som fanns i rc13.90. Tack till alla som testar den.

## Nyheter

- **Equalizers som kan kombineras i serie**: grafisk equalizer, parametrisk equalizer, hörlursequalizer och rumskorrigering följer på varandra i samma TESS-kedja; att tillämpa en rumskorrigering skriver inte längre över de andra equalizerna. Den parametriska equalizerns förstärkningsomfång är utökat.
- **Fullständig Bypass all**: en enda åtgärd kopplar bort hela TESS-kedjan och återställer den exakt som den var.
- **Hörlursplugin** hämtat från OPRA-databasen: dina hörlurars profil känns igen, med förförstärkning och nivå.
- **Speaker Setup-plugin** (inställning av högtalarna): avstånd, förstärkning och fas, kanal för kanal.
- **Uppsampling till DSD på den lokala utgången**: en egen sigma-delta-modulator, och utgångens DSD-förmågor publiceras.
- **Valbart mål för uppsamplingen**: i TESS finns en skärm där du väljer vad som ska uppsamplas till, PCM eller DSD.
- **Nivåtest av mikrofonen** före mätningen av rummet.
- **Mätning med mikrofonens inbyggda samplingsfrekvens**, från 44,1 till 192 kHz.
- **TESS-inställningar i farten**: att inaktivera ett steg får effekt direkt, och förförstärkningen förblir begränsad.

## Förbättringar

- **Rumskorrigering**: kontrollmätningen hör zonens korrigering; okorrigerade avvikelser förklaras, och den övre gränsen kan ställas in upp till 1 kHz.
- **Rumskorrigering**: inmatade värden, den senaste mätningen och den tillämpade korrigeringen finns kvar när du återvänder till skärmen; anvisningar för mikrofonens placering och en ruta ”Före mätningen” vägleder mätningen.
- **Mätning i PURE-läge**: volymen tvingas till 100 % under svepet och återställs sedan som den var.
- **Signalväg**: ett avstängt steg (uppsampling, faltning) säger varför, i TESS och på pluginets sida; i Windows i delat läge siktar uppsamplingen på mixerns samplingsfrekvens.
- **Medverkande**: instrumentet i stället för ”Musiker”, upphovspersonen skild från kompositören, ett namn per roll.
- **Skanning**: streckkoden i taggarna kompletterar albumet om det saknar en.
- **Serverstatus**: en varning visas om omslagscachen inte kan skapas.

## Rättningar

- **Kalibrering**: svepet stannar inte längre före den angivna längden och fastnar inte längre i kön; en zon utan utgång nekas tydligt.
- **Kalibrering**: en topp i taket blockerar inte längre de följande banden.
- **RT60**: ”inte mätbar (brus)” i stället för ett felaktigt värde; felmeddelanden förblir läsbara i minst 10 s.
- **Varning om samplingsfrekvens**: den visas nu bara utanför de godkända samplingsfrekvenserna.
- **Radio**: byte av station skrivs inte längre över av den tidigare stationen.
- **Omslag**: på en ny installation skapas cachen under datamappen.
- **TESS**: ett enda meddelande om ”nästa spår”, och hjälptexten för knappen ”Tillämpa” är rättad.

## Ändrat beteende

- **Ändring av TESS-kedjan under uppspelning**: ett inaktiverat steg stängs av direkt; ett tillagt eller återaktiverat steg hörs från och med nästa uppspelning.

## Att kontrollera hemma

Dessa punkter har ännu inte testats på riktig hårdvara:

- **Uppsampling till DSD på en riktig DSD-DAC**: stabilitet, nivå, inga klick.
- **Rumsmätningen vid 44,1 kHz och vid 96 kHz** på ett riktigt ljudgränssnitt, med nivåtestet av mikrofonen.
- **I Windows, med delat WASAPI**: uppspelning och uppsampling.
- **Speaker Setup på en nätverksutgång** (UPnP, AirPlay eller OAAT): avstånd, förstärkning och fas.

Och dessutom:

- **Flera equalizers i serie** med Bypass all: stämmer signalvägen med det du hör?
- **Hörlurspluginet** med dina egna hörlurar.
- **Uppdateringen från rc13.90.**

## Kända problem

- **DST-komprimerade SACD**: dessa ISO-avbilder spelas ännu inte upp direkt.
- **VU-mätare** på ett vidarebefordrat MP3-, AAC- eller Ogg-spår: efter ett hopp släcks de till spårets slut. Ljudet påverkas inte.
- **Fjärrlyssning i CarPlay**: rättningen av reläet skjuts upp till rc15.
- **Server utan skärm med autentisering avstängd**: för att byta kopplat mozaiklabs.fr-konto, aktivera autentisering eller använd en SSH-tunnel till localhost:8888.
