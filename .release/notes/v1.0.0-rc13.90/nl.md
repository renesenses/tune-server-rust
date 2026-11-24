Tune 1.0.0-rc13.90: **voorlopige versie (pre-release) van rc14**, alleen voor testers. Dit is geen gepubliceerde versie: de automatische update biedt haar niet aan. Dank aan iedereen die haar test.

## Nieuwe functies (New features)

- **TESS, de 64-bits verwerkingsketen**: alle verwerking gebeurt van begin tot eind in 64-bits drijvende komma, met één enkele kwantisering helemaal aan het einde.
- **Dezelfde weg voor alle uitgangen**: lokale uitgang, UPnP/DLNA, AirPlay, Devialet AIR en OAAT gaan door TESS; een uitgang die een bewerking niet kan ontvangen, zegt dat duidelijk.
- **Parametrische equalizer** met vrije banden (frequentie, versterking, Q), naast de grafische equalizer en de koptelefoon-equalizer (AutoEq-profielen).
- **Convolutie**: filters als .wav, .zip-archief of Convolver-configuraties (.cfg), één filter per samplefrequentie, wisselen zonder onderbreking en opgegeven latentie.
- **Ruimtecorrectie door meting**: een FIR-filter met gemengde fase, berekend uit meerdere luisterposities.
- **Upsampling naar DSD** (DSD64, DSD128, DSD256) met een sigma-deltamodulator.
- **Ketenvoorinstellingen**: sla een volledige keten op en roep haar op, meteen of bij de volgende weergave.
- **Signaalpad**: vanuit de afspeelbalk, trap voor trap, wat werkelijk wordt toegepast en waarom een trap niet.
- **Native plug-ins** in de keten: inschakelen tijdens gebruik, 64-bits formaat, volgorde en aantal kanalen gerespecteerd.
- **Interface in 20 talen**: Nederlands, Traditioneel Chinees, Russisch en Oekraïens komen erbij.
- **Authenticatie**: een knop "Authenticatie uitschakelen" in Instellingen › Systeem › Authenticatie.

## Verbeteringen (Improvements)

- **SACD-ISO**: native DSD wordt aan netwerkspelers geleverd als virtuele DSF; DSD via DoP meldt zich als DSD.
- **Luisteren op afstand**: via 5G gaat het eerste nummer als FLAC.
- **Afwezige apparaten**: na 30 dagen automatisch vergeten (instelbaar), zonder ooit een zone te verwijderen.
- **Cover+**: betere hoezen, ook voor webradio's (ICY-metadata).
- **Groepsweergave**: instelbare vertraging per zone; "Vorige" in de leidende zone herstart ook de volgende zones.
- **Bibliotheek en streaming**: achtergrondtaken blokkeren de interface niet meer, de alfabetische albumsortering blijft behouden, vergelijkbare artiesten vanaf de artiestenpagina van een dienst.
- **Weigeringsmeldingen** zijn korter en verwijzen naar de juiste instelling.

## Oplossingen (Fixes)

- **De oplossingen van rc13.1 en rc13.2** zijn inbegrepen: aanmelden met het beheerdersaccount, het mozaiklabs.fr-account koppelen vanaf het lokale netwerk, hoezen met actieve authenticatie, bijwerken onder Windows.
- **UPnP**: Tune indexeert zichzelf niet meer; een geïndexeerd nummer speelt vanaf het huidige adres van zijn server.
- **Weergave**: 30 s vooraf laden zonder weggegooide buffers; een sprong die de bron weigert, verschuift de stream niet meer.
- **Chromecast** maakt opnieuw verbinding; opnieuw publiceren via AirPlay onderbreekt de weergave niet meer.
- **Podcasts**: niet-beschikbare afleveringen worden netjes gemeld.
- **Meldingen uit het forum**: geschiedenis, Qobuz, Spotify, knop "Terug", zoeken en iPad.

## Gewijzigd gedrag

- **Parametrische equalizer**: gratis tot rc13, nu onderdeel van Tune Premium, net als convolutie, ruimtecorrectie, realtime declick en plug-ins van derden. De grafische equalizer, de koptelefoon-equalizer en crossfeed blijven gratis.

## Controleer bij u thuis

- **Een volledige TESS-keten** (equalizer, crossfeed, convolutie of ruimtecorrectie): komt het signaalpad overeen met wat u hoort?
- **Een convolutieset importeren** en tijdens de weergave van filter wisselen.
- **Upsampling naar DSD** op een compatibele DAC: stabiliteit, niveau, geen klikken.
- **Ketenvoorinstellingen**, in beide toepassingsmodi.
- **Een netwerkuitgang** (UPnP, AirPlay of OAAT) met actieve bewerking.
- **Bijwerken vanaf rc13.x** en de knop "Authenticatie uitschakelen".

## Bekende problemen

- **DST-gecomprimeerde SACD's**: deze ISO-images worden nog niet native afgespeeld.
- **VU-meters** op een doorgegeven MP3-, AAC- of Ogg-nummer: na een sprong schakelen ze uit tot het einde van het nummer. Het geluid wordt niet beïnvloed.
- **Luisteren op afstand via CarPlay**: de correctie van het relais is uitgesteld tot rc15.
- **Server zonder scherm met authenticatie uit**: om het gekoppelde mozaiklabs.fr-account te wijzigen, schakel authenticatie in of gebruik een SSH-tunnel naar localhost:8888.
