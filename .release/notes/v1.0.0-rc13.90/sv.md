Tune 1.0.0-rc13.90: **förhandsversion (pre-release) av rc14**, endast för testare. Det här är ingen publicerad version: den automatiska uppdateringen erbjuder den inte. Tack till alla som testar den.

## Nyheter

- **TESS, den 64-bitars bearbetningskedjan**: all bearbetning sker i 64-bitars flyttal hela vägen, med en enda kvantisering allra sist.
- **Samma väg för alla utgångar**: lokal utgång, UPnP/DLNA, AirPlay, Devialet AIR och OAAT går genom TESS; en utgång som inte kan ta emot en bearbetning säger det tydligt.
- **Parametrisk equalizer** med fria band (frekvens, förstärkning, Q), bredvid den grafiska equalizern och hörlursequalizern (AutoEq-profiler).
- **Faltning**: filter som .wav, .zip-arkiv eller Convolver-konfigurationer (.cfg), ett filter per samplingsfrekvens, byte utan avbrott och angiven latens.
- **Rumskorrigering genom mätning**: ett FIR-filter med blandad fas beräknat från flera lyssningspositioner.
- **Uppsampling till DSD** (DSD64, DSD128, DSD256) med en sigma-delta-modulator.
- **Kedjeförval**: spara en hel kedja och hämta den, direkt eller vid nästa uppspelning.
- **Signalväg**: från uppspelningsfältet, steg för steg, vad som verkligen tillämpas och varför ett steg inte gör det.
- **Inbyggda plugin** i kedjan: aktivering under drift, 64-bitarsformat, kanalernas ordning och antal respekteras.
- **Gränssnitt på 20 språk**: nederländska, traditionell kinesiska, ryska och ukrainska tillkommer.
- **Autentisering**: en knapp ”Inaktivera autentisering” under Inställningar › System › Autentisering.

## Förbättringar

- **SACD-ISO**: inbyggd DSD skickas till nätverksspelare som virtuell DSF; DSD som överförs via DoP anmäls som DSD.
- **Fjärrlyssning**: över 5G skickas första spåret som FLAC.
- **Frånvarande enheter**: glöms automatiskt efter 30 dagar (justerbart), utan att någonsin ta bort en zon.
- **Cover+**: bättre omslag, även för webbradio (ICY-metadata).
- **Gruppuppspelning**: justerbar fördröjning per zon; ”Föregående” i den ledande zonen startar även om de följande zonerna.
- **Bibliotek och streaming**: bakgrundskörningar blockerar inte längre gränssnittet, alfabetisk albumsortering behålls, liknande artister från en tjänsts artistsida.
- **Avvisningsmeddelanden** är kortare och pekar på rätt inställning.

## Rättningar

- **Rättningarna i rc13.1 och rc13.2** ingår: inloggning på administratörskontot, koppling av mozaiklabs.fr-kontot från det lokala nätverket, omslag med aktiv autentisering, uppdatering i Windows.
- **UPnP**: Tune indexerar inte längre sig självt; ett indexerat spår spelas från serverns aktuella adress.
- **Uppspelning**: 30 s förladdning utan kasserade buffertar; ett hopp som källan nekar förskjuter inte längre strömmen.
- **Chromecast** återansluter; ompublicering i AirPlay avbryter inte längre uppspelningen.
- **Poddar**: otillgängliga avsnitt rapporteras korrekt.
- **Rapporter från forumet**: historik, Qobuz, Spotify, ”Tillbaka”-knappen, sökning och iPad.

## Ändrat beteende

- **Parametrisk equalizer**: gratis fram till rc13, ingår nu i Tune Premium, liksom faltning, rumskorrigering, declick i realtid och plugin från tredje part. Den grafiska equalizern, hörlursequalizern och crossfeed förblir gratis.

## Att kontrollera hemma

- **En komplett TESS-kedja** (equalizer, crossfeed, faltning eller rumskorrigering): stämmer signalvägen med det du hör?
- **Import av en faltningsuppsättning** och filterbyte under uppspelning.
- **Uppsampling till DSD** på en kompatibel DAC: stabilitet, nivå, inga klick.
- **Kedjeförval**, i båda tillämpningslägena.
- **En nätverksutgång** (UPnP, AirPlay eller OAAT) med aktiv bearbetning.
- **Uppdatering från rc13.x** och knappen ”Inaktivera autentisering”.

## Kända problem

- **DST-komprimerade SACD**: dessa ISO-avbilder spelas ännu inte upp direkt.
- **VU-mätare** på ett vidarebefordrat MP3-, AAC- eller Ogg-spår: efter ett hopp släcks de till spårets slut. Ljudet påverkas inte.
- **Fjärrlyssning i CarPlay**: rättningen av reläet skjuts upp till rc15.
- **Server utan skärm med autentisering avstängd**: för att byta kopplat mozaiklabs.fr-konto, aktivera autentisering eller använd en SSH-tunnel till localhost:8888.
