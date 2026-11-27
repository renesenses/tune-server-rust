Tune 1.0.0-rc14 bevat de volledige inhoud van de pre-releases rc13.90 en rc13.91; deze notities beschrijven wat er sinds rc13.91 is veranderd. Dank aan de testers: veel van deze punten komen uit uw feedback.

## Nieuwe functies (New features)

- **TESS, voorinstellingen Opslaan en Opnieuw laden**: zodra een instelling afwijkt van de gekozen voorinstelling, toont TESS „gewijzigd”. **Opslaan** overschrijft de oorspronkelijke voorinstelling, **Opnieuw laden** keert ernaar terug. Een voorinstelling die met Tune wordt meegeleverd, zoals „Pure”, wordt niet overschreven: „Opslaan als…” maakt een kopie.
- **TESS, DSD-voorinstelling direct toegepast**: een voorinstelling die het DSD-doel wijzigt (bijvoorbeeld DSD128 → DSD256) en in de modus „direct toepassen” wordt geladen, start het nummer opnieuw op zijn positie, in plaats van op het volgende nummer te wachten. Upsampling is een Premium-functie.
- **Headroom**: Automatisch, of met de hand van −12 tot 0 dB in stappen van 0,5 dB. Het paneel toont de werkelijk toegepaste waarde, of zegt waarom die niet wordt toegepast.
- **Headroom bij upsampling naar DSD**: de handmatige headroom wordt niet meer twee keer toegepast (−12 dB gaf −24 dB; nu is het −15 dB, waarvan −3 dB voor de DSD-modulator, getoond op een eigen regel).
- **Ruimtemeting met begeleiding naar 75 dB(C)**: met een gekalibreerde microfoon toont het scherm het gemeten niveau, het verschil met 75 dB(C) en een volume-instructie. De regel is voorzichtig, omdat de omrekening nog niet met een kalibrator is gecontroleerd: Tune raadt nooit aan boven een geschat niveau van 80 dB(C) te gaan en schakelt het signaal uit bij 90 dB SPL. Correctie gemeten met een microfoon is Premium; correctie berekend zonder microfoon blijft gratis.
- **Gehoorbescherming**: een equalizer uitvinken, overschakelen naar PURE of Bypass all, of een convolutie, een crossfeed of een mono-downmix verwijderen, laat het niveau niet meer ineens stijgen: het keert terug naar het vorige niveau met een fade van ongeveer een halve seconde. Hebt u intussen het volume verhoogd, dan gaat het lampje „niveau verlaagd” branden en geeft „Niveau herstellen” het teveel vrij. Dit geldt ook voor de netwerkuitgangen (UPnP, OAAT).
- **Getrouw signaalpad**:
  - op een netwerkuitgang volgt het de werkelijke volgorde van de stream, tot aan het volume van de speler;
  - de trap van een impulsresponsfilter heet „Convolutie”;
  - het filter behoudt de naam van het geïmporteerde .wav- of .zip-bestand;
  - na een wijziging van het DSD-doel toont het de DSD-frequentie die werkelijk speelt, en kondigt het de volgende aan;
  - het venster wordt groen: oranje is voorbehouden aan echte waarschuwingen.
- **Convolutie**: het paneel toont de curve van het geladen filter, kanaal per kanaal, en op verzoek de fase.
- **Bypass all**: het TESS-pictogram in de afspeelbalk wordt gedimd als TESS wordt omzeild, en de TESS-pagina toont „TESS omzeild”.
- **Geharmoniseerde TESS-pagina**: accentkleur TESS-groen, huidige zone naast de badge, paneel Headroom grijs op een netwerkuitgang, en meting „Zonder microfoon (schatting)” of „Met microfoon (aanbevolen)”.
- **Netwerkuitgang**: de kaart Upsampling zegt dat upsampling daar niet geldt.
- **Geharmoniseerde interface**: ingebouwde lettertypen, ook offline; Bibliotheek, Live Radio, Mediaservers en Concerten volgen de nieuwe huisstijl.

## Oplossingen (Fixes)

- **Radio naar een Sonos** (en elke netwerkspeler met een frequentieplafond): een radiozender in 192 kHz wordt geleverd op de frequentie die de speler accepteert. Voorheen stopte de speler meteen, telkens opnieuw.
- **768 kHz**: de lijst „Max. frequentie” van een zone biedt 768 kHz en 1.536 kHz.
- **Bibliotheek**: het albumraster bevriest niet meer tijdens het scrollen door een grote collectie.
- **Afspelen in het browsertabblad**: verspringen binnen het nummer verplaatst het afspelen echt, ook vanaf een ander apparaat.
- **Help**: de banner over prioriteitssupport verschijnt niet meer bij een Premium-account.
- **Licentie**: de vervaldatum wordt correct gelezen, en Instellingen › Licentie zegt waarom het niveau gratis is.
- **Convolutie**: een filter dat sommige frequenties versterkt, clipt niet meer.
- **Lange podcasts**: een aflevering waarvan de host de verbinding regelmatig verbreekt, wordt niet meer afgebroken.
- **Debian-pakket in een LXC-container** (Proxmox): de dienst start ook zonder private mount-namespaces.
- **Eerste start**: de wizard noemt diensten zonder account niet meer „ingeschakeld”, en slaat de gekozen muziekmap op. Een lege „Mijn radiozenders” opent de zendergids.

## Beveiliging en accounts

- **Vernieuwde sessie**: met „Ingelogd blijven” (standaard uitgevinkt) vernieuwt de browsersessie zich zolang ze wordt gebruikt, tot 90 dagen. Anders, zoals voorheen, 24 uur. Het avatarmenu biedt „Afmelden bij deze server”.
- **Beheerderstoegang herstellen**: voer `tune-server --reset-admin` (of `--reinitialiser-admin`) uit **op de servermachine**. De verkregen eenmalige code, 15 minuten geldig, voert u in bij „Wachtwoord vergeten?” op deze machine; zonder browser voegt u `--password-stdin` toe. Open sessies worden gesloten. De installatiehandleiding (sectie 9) geeft de opdracht voor elk systeem.
- **Profiel `admin`**: het standaard aangemaakte beheerdersprofiel heet nu `admin`; een gekozen naam blijft behouden, en `default` wordt nog steeds geaccepteerd.
- **Beheerderswachtwoord vanaf de eerste configuratie**: bij een nieuwe installatie laat de wizard u het aanmaken.
- **Server zonder authenticatie**: beheer wordt geaccepteerd vanuit het lokale netwerk, niet meer alleen vanaf de servermachine. **Waarschuwing**: zonder authenticatie kunnen de apparaten in het lokale netwerk de server beheren; schakel authenticatie in als uw netwerk niet te vertrouwen is. De instelling „Beheer alleen vanaf deze machine” herstelt de vorige regel.

## Afstandsbedieningen

- Op een beveiligde server vragen apps die nog niet zijn bijgewerkt, zoals voorheen elke 24 uur opnieuw om het wachtwoord.

## Controleer bij u thuis

- **„Bypass all” in een voorinstelling**: een voorinstelling die zowel Bypass all als het DSD-doel wijzigt, start in de modus „direct toepassen” het nummer opnieuw op zijn positie. Alleen op de testbank gecontroleerd: laat ons weten of de hervatting schoon is.
