Tune 1.0.0-rc13.91: **a doua pre-release a rc14**, rezervată testerilor. Nu este o versiune publicată: actualizarea automată nu o propune. Preia tot conținutul rc13.90. Mulțumim celor care o testează.

## Noutăți

- **Egalizatoare combinabile în serie**: egalizatorul grafic, cel parametric, cel pentru căști și corecția camerei se înlănțuie în același lanț TESS; aplicarea unei corecții a camerei nu mai suprascrie celelalte egalizatoare. Câștigurile egalizatorului parametric sunt extinse.
- **Bypass all complet**: un singur gest scoate din circuit întregul lanț TESS și îl restabilește identic.
- **Plugin Căști** preluat din baza de date OPRA: profilul căștilor dumneavoastră este recunoscut, cu preamplificare și nivel.
- **Plugin Speaker Setup** (reglajul boxelor): distanță, câștig și fază, canal cu canal.
- **Upsampling către DSD pe ieșirea locală**: un modulator sigma-delta dedicat, iar capacitățile DSD ale ieșirii sunt publicate.
- **Ținta upsampling-ului la alegere**: în TESS, un ecran permite alegerea formatului către care se face upsampling-ul, PCM sau DSD.
- **Test de nivel al microfonului** înainte de măsurarea camerei.
- **Măsurare la frecvența nativă a microfonului**, de la 44,1 la 192 kHz.
- **Setări TESS din mers**: dezactivarea unei etape are efect imediat, iar preamplificarea rămâne limitată.

## Îmbunătățiri

- **Corecția camerei**: măsurarea de verificare aude corecția zonei; abaterile necorectate sunt explicate, iar limita superioară se poate regla până la 1 kHz.
- **Corecția camerei**: datele introduse, ultima măsurare și corecția aplicată sunt regăsite la revenirea pe ecran; instrucțiuni de amplasare a microfonului și o casetă „Înainte de măsurare” ghidează măsurarea.
- **Măsurare în modul PURE**: volumul este forțat la 100 % pe durata baleiajului, apoi readus cum era.
- **Calea semnalului**: o etapă oprită (upsampling, convoluție) spune de ce, în TESS și pe pagina pluginului; pe Windows în modul partajat, upsampling-ul țintește frecvența mixerului.
- **Credite**: instrumentul în loc de „Muzician”, autorul distinct de compozitor, un nume pe rol.
- **Scanare**: codul de bare prezent în etichete completează albumul dacă acesta nu are unul.
- **Starea serverului**: se afișează un avertisment dacă memoria cache a coperților nu poate fi creată.

## Corecturi

- **Calibrare**: baleiajul nu se mai oprește înainte de durata anunțată și nu mai rămâne blocat în coadă; o zonă fără ieșire este refuzată clar.
- **Calibrare**: un vârf la plafon nu mai blochează benzile următoare.
- **RT60**: „nemăsurabil (zgomot)” în loc de o valoare greșită; mesajele de eroare rămân lizibile cel puțin 10 s.
- **Avertisment de frecvență**: apare acum doar în afara frecvențelor acceptate.
- **Radio**: schimbarea postului nu mai este suprascrisă de postul anterior.
- **Coperți**: pe o instalare nouă, memoria cache este creată în dosarul de date.
- **TESS**: un singur aviz „piesa următoare”, iar ajutorul butonului „Aplică” este corectat.

## Schimbări de comportament

- **Schimbarea lanțului TESS în timpul redării**: o etapă dezactivată se oprește imediat; o etapă adăugată sau reactivată se aude de la următoarea redare.

## De verificat la dumneavoastră

Aceste puncte nu au fost încă testate pe hardware real:

- **Upsampling-ul către DSD pe un DAC DSD real**: stabilitate, nivel, absența clicurilor.
- **Măsurarea camerei la 44,1 kHz și la 96 kHz** pe o interfață audio reală, cu testul de nivel al microfonului.
- **Pe Windows, în WASAPI partajat**: redare și upsampling.
- **Speaker Setup pe o ieșire de rețea** (UPnP, AirPlay sau OAAT): distanță, câștig și fază.

Și de asemenea:

- **Mai multe egalizatoare în serie** cu Bypass all: calea semnalului corespunde cu ceea ce auziți?
- **Pluginul Căști** cu propriile dumneavoastră căști.
- **Actualizarea de la rc13.90.**

## Probleme cunoscute

- **SACD comprimate în DST**: aceste imagini ISO nu sunt încă redate nativ.
- **VU-metrele** pe o piesă MP3, AAC sau Ogg retransmisă: după un salt, se opresc până la sfârșitul piesei. Sunetul nu este afectat.
- **Ascultare la distanță pe CarPlay**: corecția releului este amânată pentru rc15.
- **Server fără ecran, cu autentificarea oprită**: pentru a schimba contul mozaiklabs.fr legat, activați autentificarea sau folosiți un tunel SSH către localhost:8888.
