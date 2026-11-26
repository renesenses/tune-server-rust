Tune 1.0.0-rc13.91: **tweede voorlopige versie (pre-release) van rc14**, alleen voor testers. Dit is geen gepubliceerde versie: de automatische update biedt haar niet aan. Ze bevat alle inhoud van rc13.90. Dank aan iedereen die haar test.

## Nieuwe functies (New features)

- **Equalizers in serie te combineren**: grafische, parametrische en koptelefoon-equalizer en ruimtecorrectie volgen elkaar op in dezelfde TESS-keten; een ruimtecorrectie toepassen overschrijft de andere equalizers niet meer. Het versterkingsbereik van de parametrische equalizer is verruimd.
- **Volledige Bypass all**: één handeling zet de hele TESS-keten buiten werking en herstelt haar precies zoals ze was.
- **Koptelefoon-plug-in** op basis van de OPRA-database: het profiel van uw koptelefoon wordt herkend, met voorversterking en niveau.
- **Speaker Setup-plug-in** (luidsprekerinstelling): afstand, versterking en fase, kanaal per kanaal.
- **Upsampling naar DSD op de lokale uitgang**: een eigen sigma-deltamodulator, en de DSD-mogelijkheden van de uitgang worden gepubliceerd.
- **Keuze van het upsamplingdoel**: in TESS kies je op een scherm waarnaar wordt geüpsampled, PCM of DSD.
- **Niveautest van de microfoon** vóór de ruimtemeting.
- **Meting op de eigen samplefrequentie van de microfoon**, van 44,1 tot 192 kHz.
- **TESS-instellingen tijdens gebruik**: een trap uitschakelen werkt meteen, en de voorversterking blijft begrensd.

## Verbeteringen (Improvements)

- **Ruimtecorrectie**: de controlemeting hoort de correctie van de zone; niet-gecorrigeerde afwijkingen worden uitgelegd, en de bovengrens is instelbaar tot 1 kHz.
- **Ruimtecorrectie**: de invoer, de laatste meting en de toegepaste correctie zijn er weer wanneer u naar het scherm terugkeert; aanwijzingen voor de plaatsing van de microfoon en een kader "Vóór de meting" begeleiden de meting.
- **Meting in PURE-modus**: het volume wordt tijdens de sweep op 100% gezet en daarna teruggezet zoals het was.
- **Signaalpad**: een uitgeschakelde trap (upsampling, convolutie) zegt waarom, in TESS en op de pagina van de plug-in; onder Windows in gedeelde modus mikt de upsampling op de frequentie van de mixer.
- **Credits**: het instrument in plaats van "Muzikant", de auteur los van de componist, één naam per rol.
- **Scan**: de barcode uit de tags vult het album aan als het er geen heeft.
- **Serverstatus**: er verschijnt een waarschuwing als de cache voor hoezen niet kan worden aangemaakt.

## Oplossingen (Fixes)

- **Kalibratie**: de sweep stopt niet meer vóór de aangekondigde lengte en blijft niet meer hangen in de wachtrij; een zone zonder uitgang wordt duidelijk geweigerd.
- **Kalibratie**: een piek tegen het plafond blokkeert de volgende banden niet meer.
- **RT60**: "niet meetbaar (ruis)" in plaats van een verkeerde waarde; foutmeldingen blijven minstens 10 s leesbaar.
- **Frequentiewaarschuwing**: verschijnt alleen nog buiten de geaccepteerde frequenties.
- **Radio**: het wisselen van zender wordt niet meer overschreven door de vorige zender.
- **Hoezen**: bij een nieuwe installatie wordt de cache onder de gegevensmap aangemaakt.
- **TESS**: één enkele melding "volgend nummer", en de helptekst van de knop "Toepassen" is verbeterd.

## Gewijzigd gedrag

- **TESS-keten wijzigen tijdens de weergave**: een uitgeschakelde trap valt meteen weg; een toegevoegde of opnieuw ingeschakelde trap is te horen vanaf de volgende weergave.

## Controleer bij u thuis

Deze punten zijn nog niet op echte apparatuur getest:

- **Upsampling naar DSD op een echte DSD-DAC**: stabiliteit, niveau, geen klikken.
- **De ruimtemeting op 44,1 kHz en op 96 kHz** met een echte audio-interface, met de niveautest van de microfoon.
- **Onder Windows, in gedeelde WASAPI-modus**: weergave en upsampling.
- **Speaker Setup op een netwerkuitgang** (UPnP, AirPlay of OAAT): afstand, versterking en fase.

En ook:

- **Meerdere equalizers in serie** met Bypass all: komt het signaalpad overeen met wat u hoort?
- **De Koptelefoon-plug-in** met uw eigen koptelefoon.
- **Bijwerken vanaf rc13.90.**

## Bekende problemen

- **DST-gecomprimeerde SACD's**: deze ISO-images worden nog niet native afgespeeld.
- **VU-meters** op een doorgegeven MP3-, AAC- of Ogg-nummer: na een sprong schakelen ze uit tot het einde van het nummer. Het geluid wordt niet beïnvloed.
- **Luisteren op afstand via CarPlay**: de correctie van het relais is uitgesteld tot rc15.
- **Server zonder scherm met authenticatie uit**: om het gekoppelde mozaiklabs.fr-account te wijzigen, schakel authenticatie in of gebruik een SSH-tunnel naar localhost:8888.
