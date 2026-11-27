Tune 1.0.0-rc14 innehåller allt från förhandsversionerna rc13.90 och rc13.91; de här anteckningarna beskriver vad som har ändrats sedan rc13.91. Tack till testarna: många av punkterna kommer från er återkoppling.

## Nyheter

- **TESS, förinställningar Spara och Läs in igen**: så snart en inställning avviker från den valda förinställningen visar TESS ”ändrad”. **Spara** skriver över den ursprungliga förinställningen, **Läs in igen** återgår till den. En förinställning som följer med Tune, som ”Pure”, skrivs inte över: ”Spara som…” gör en kopia.
- **TESS, DSD-förinställning tillämpas direkt**: en förinställning som ändrar DSD-målet (till exempel DSD128 → DSD256), inläst i läget ”tillämpa direkt”, startar om spåret vid dess position i stället för att vänta på nästa spår. Uppsampling är en Premium-funktion.
- **Headroom**: Automatiskt, eller för hand från −12 till 0 dB i steg om 0,5 dB. Panelen visar det värde som faktiskt tillämpas, eller säger varför det inte gör det.
- **Headroom vid uppsampling till DSD**: den manuella headroomen tillämpas inte längre två gånger (−12 dB gav −24 dB; nu är det −15 dB, varav −3 dB för DSD-modulatorn, som visas på en egen rad).
- **Rumsmätning med vägledning mot 75 dB(C)**: med en kalibrerad mikrofon visar skärmen den uppmätta nivån, avvikelsen från 75 dB(C) och en volyminstruktion. Regeln är försiktig, eftersom omräkningen ännu inte har kontrollerats mot en kalibrator: Tune råder aldrig dig att gå över uppskattade 80 dB(C) och bryter signalen vid 90 dB SPL. Korrigering mätt med mikrofon är Premium; korrigering beräknad utan mikrofon förblir gratis.
- **Hörselskydd**: att avmarkera en equalizer, växla till PURE eller Bypass all, eller ta bort en faltning, en crossfeed eller en mononedmixning får inte längre nivån att stiga plötsligt: den återgår till den tidigare nivån med en övertoning på ungefär en halv sekund. Om du har höjt volymen under tiden tänds indikatorn ”nivå sänkt” och ”Återställ nivån” släpper överskottet. Det gäller även nätverksutgångarna (UPnP, OAAT).
- **En trogen signalväg**:
  - på en nätverksutgång följer den strömmens verkliga ordning, ända till spelarens volym;
  - steget för ett impulssvarsfilter heter ”Faltning”;
  - filtret behåller namnet på den importerade .wav- eller .zip-filen;
  - efter ett byte av DSD-mål visar den den DSD-frekvens som faktiskt spelas, och meddelar nästa;
  - fönstret blir grönt: orange är reserverat för verkliga varningar.
- **Faltning**: panelen visar kurvan för det inlästa filtret, kanal för kanal, och fasen på begäran.
- **Bypass all**: TESS-ikonen i uppspelningsfältet dämpas när TESS är förbikopplat, och TESS-sidan visar ”TESS förbikopplat”.
- **Harmoniserad TESS-sida**: accent i TESS-grönt, aktuell zon bredvid märket, panelen Headroom nedtonad på en nätverksutgång, och mätning ”Utan mikrofon (uppskattning)” eller ”Med mikrofon (rekommenderas)”.
- **Nätverksutgång**: kortet Uppsampling säger att uppsampling inte tillämpas där.
- **Harmoniserat gränssnitt**: inbäddade typsnitt, även offline; Bibliotek, Live Radio, Mediaservrar och Konserter följer den nya grafiska profilen.

## Rättningar

- **Radio till en Sonos** (och varje nätverksspelare med ett tak för samplingsfrekvensen): en radiostation i 192 kHz levereras med den frekvens som spelaren accepterar. Tidigare stannade spelaren direkt, om och om igen.
- **768 kHz**: listan ”Maxfrekvens” för en zon erbjuder 768 kHz och 1 536 kHz.
- **Bibliotek**: albumrutnätet fryser inte längre när du rullar genom en stor samling.
- **Uppspelning i webbläsarfliken**: att spola i spåret flyttar verkligen uppspelningen, även från en annan enhet.
- **Hjälp**: banderollen om prioriterad support visas inte längre för ett Premium-konto.
- **Licens**: utgångsdatumet läses korrekt, och Inställningar › Licens säger varför nivån är gratis.
- **Faltning**: ett filter som förstärker vissa frekvenser klipper inte längre.
- **Långa poddar**: ett avsnitt vars värd regelbundet bryter anslutningen avbryts inte längre.
- **Debian-paket i en LXC-container** (Proxmox): tjänsten startar även utan privata monteringsnamnrymder.
- **Första start**: guiden kallar inte längre tjänster utan konto för ”aktiverade”, och den sparar den valda musikmappen. En tom ”Mina radiokanaler” öppnar katalogen.

## Säkerhet och konton

- **Förnyad session**: med ”Förbli inloggad” (avmarkerad som standard) förnyas webbläsarsessionen så länge den används, upp till 90 dagar. Annars 24 h som tidigare. Avatarmenyn erbjuder ”Logga ut från den här servern”.
- **Återfå administratörsåtkomst**: kör `tune-server --reset-admin` (eller `--reinitialiser-admin`) **på servermaskinen**. Engångskoden du får, giltig i 15 minuter, anges under ”Glömt lösenordet?” på den maskinen; utan webbläsare lägger du till `--password-stdin`. Öppna sessioner stängs. Installationsguiden (avsnitt 9) anger kommandot för varje system.
- **Profilen `admin`**: administratörsprofilen som skapas som standard heter nu `admin`; ett valt namn behålls, och `default` godtas fortfarande.
- **Administratörslösenord från första konfigurationen**: på en ny installation låter guiden dig skapa det.
- **Server utan autentisering**: administration godtas från det lokala nätverket, inte längre bara från servermaskinen. **Varning**: utan autentisering kan enheterna i det lokala nätverket administrera servern; aktivera autentisering om ditt nätverk inte är betrott. Inställningen ”Administration endast från den här datorn” återställer den tidigare regeln.

## Fjärrkontroller

- På en skyddad server ber appar som ännu inte är uppdaterade om lösenordet igen var 24:e timme, som tidigare.

## Att kontrollera hemma

- **”Bypass all” i en förinställning**: en förinställning som ändrar både Bypass all och DSD-målet, i läget ”tillämpa direkt”, startar om spåret vid dess position. Bara kontrollerat i testbänk: berätta för oss om återupptagningen är ren.
