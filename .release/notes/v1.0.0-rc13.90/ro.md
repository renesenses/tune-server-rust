Tune 1.0.0-rc13.90: **pre-release a rc14**, rezervată testerilor. Nu este o versiune publicată: actualizarea automată nu o propune. Mulțumim celor care o testează.

## Noutăți

- **TESS, lanțul de procesare pe 64 de biți**: toată procesarea se face în virgulă mobilă pe 64 de biți de la un capăt la altul, cu o singură cuantizare, chiar la final.
- **Același drum pentru toate ieșirile**: ieșirea locală, UPnP/DLNA, AirPlay, Devialet AIR și OAAT trec prin TESS; o ieșire care nu poate primi o procesare o spune clar.
- **Egalizator parametric** cu benzi libere (frecvență, câștig, Q), alături de egalizatorul grafic și de egalizatorul pentru căști (profiluri AutoEq).
- **Convoluție**: filtre .wav, arhive .zip sau configurații Convolver (.cfg), un filtru pe frecvență de eșantionare, schimbare fără întrerupere și latență anunțată.
- **Corecția camerei prin măsurare**: un filtru FIR cu fază mixtă calculat din mai multe poziții de ascultare.
- **Upsampling către DSD** (DSD64, DSD128, DSD256) cu un modulator sigma-delta.
- **Presetări de lanț**: salvați un lanț complet și reapelați-l, imediat sau la următoarea redare.
- **Calea semnalului**: din bara de redare, etapă cu etapă, ce se aplică de fapt și de ce o etapă nu se aplică.
- **Pluginuri native** în lanț: activare din mers, format pe 64 de biți, ordinea și numărul canalelor respectate.
- **Interfață în 20 de limbi**: sosesc olandeza, chineza tradițională, rusa și ucraineana.
- **Autentificare**: un buton „Dezactivează autentificarea” în Setări › Sistem › Autentificare.

## Îmbunătățiri

- **ISO SACD**: DSD-ul nativ este servit playerelor de rețea ca DSF virtual; un DSD transportat în DoP se anunță ca DSD.
- **Ascultare la distanță**: în 5G, prima piesă pleacă în FLAC.
- **Dispozitive absente**: uitate automat după 30 de zile (reglabil), fără a șterge vreodată o zonă.
- **Cover+**: coperți mai bune, și pentru radiourile web (metadate ICY).
- **Redare în grup**: decalaj reglabil pe zonă; „Anterior” în zona principală repornește și zonele care o urmează.
- **Bibliotecă și streaming**: trecerile în fundal nu mai blochează interfața, ordinea alfabetică a albumelor este păstrată, artiști similari din fișa unui serviciu.
- **Mesaje de refuz** mai scurte, care trimit la setarea potrivită.

## Corecturi

- **Corecturile din rc13.1 și rc13.2** sunt incluse: conectarea contului de administrator, legarea contului mozaiklabs.fr din rețeaua locală, coperțile cu autentificarea activă, actualizarea pe Windows.
- **UPnP**: Tune nu se mai indexează pe sine; o piesă indexată se redă de la adresa actuală a serverului ei.
- **Redare**: preîncărcare de 30 s fără tampoane aruncate; un salt refuzat de sursă nu mai decalează fluxul.
- **Chromecast** se reconectează; republicarea AirPlay nu mai întrerupe redarea.
- **Podcasturi**: episoadele indisponibile sunt semnalate corect.
- **Semnalări de pe forum**: istoric, Qobuz, Spotify, butonul „Înapoi”, căutare și iPad.

## Schimbări de comportament

- **Egalizatorul parametric**: gratuit până la rc13, face acum parte din Tune Premium, ca și convoluția, corecția camerei, declick-ul în timp real și pluginurile terțe. Egalizatorul grafic, egalizatorul pentru căști și crossfeed-ul rămân gratuite.

## De verificat la dumneavoastră

- **Un lanț TESS complet** (egalizator, crossfeed, convoluție sau corecția camerei): calea semnalului corespunde cu ce auziți?
- **Importul unui set de convoluție** și schimbarea filtrului în timpul redării.
- **Upsampling-ul către DSD** pe un DAC compatibil: stabilitate, nivel, fără clicuri.
- **Presetările de lanț**, în cele două moduri de aplicare.
- **O ieșire de rețea** (UPnP, AirPlay sau OAAT) cu o procesare activă.
- **Actualizarea de la rc13.x** și butonul „Dezactivează autentificarea”.

## Probleme cunoscute

- **SACD comprimate în DST**: aceste imagini ISO nu sunt încă redate nativ.
- **VU-metrele** pe o piesă MP3, AAC sau Ogg retransmisă: după un salt, se opresc până la sfârșitul piesei. Sunetul nu este afectat.
- **Ascultare la distanță pe CarPlay**: corecția releului este amânată pentru rc15.
- **Server fără ecran, cu autentificarea oprită**: pentru a schimba contul mozaiklabs.fr legat, activați autentificarea sau folosiți un tunel SSH către localhost:8888.
