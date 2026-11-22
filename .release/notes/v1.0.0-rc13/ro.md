Tune 1.0.0-rc13, a treisprezecea versiune candidată a Tune 1.0. Mulțumim lui Gros Bidon, r5yk5, Tades, cpadiou77, Pierre, Daniel LEVY, Lio, sashasha, ajm, Jean Valjean, Semakawa, Jon Rotaetxe, hifi25nl, aqs, Christopher J, FabienM, Begastor și Régis pentru semnalări și teste, lui alec_eiffel pentru Smart Upsampler, lui cometdom pentru Qobuz și lui Levente pentru design.

## Noutăți

- **Supraeșantionator**: Smart Upsampler, conceput de Rémi (alec_eiffel), cântă la același nivel ca fără el, iar o modificare a setărilor se aude imediat.
- **Redare și ieșiri**: Sendspin și Devialet AIR primesc sunetul procesat (egalizator, pluginuri, supraeșantionator). Oprirea programată poate închide computerul; comenzi multimedia ale desktopului pe Linux.
- **Crossfeed**: gratuit, cu setările și presetările sale.
- **Convoluție**: câte un fișier de răspuns la impuls pentru fiecare frecvență de eșantionare.
- **DSD**: DSD-ul de pe un server UPnP se redă nativ pe playerele de rețea pretențioase, iar două piese DSD se succed fără pauză.
- **Bibliotecă**: etichetele dumneavoastră personale devin etichete Tune; selecția mai multor albume pentru o acțiune grupată; filtru după instrument.
- **Sursă alterată** înlocuiește „Hi-Res îndoielnic”, pentru toate formatele, cu frecvența de tăiere măsurată.
- **Formă de undă** mai fină, și pentru piesele CUE, Qobuz și TIDAL (opțional).
- **Coadă**: albumele se restrâng și se mută dintr-un singur gest.
- **Acasă**: 5 playlisturi „Pentru că îți place…” și widgeturile Cei mai ascultați artiști, Ascultate frecvent, Ascultă mai târziu și Reluați ascultarea.
- **Servicii**: pagina „Pentru tine” a TIDAL; 200 de noutăți Qobuz pe gen.
- **Tune Circle**: membrii unui cerc se văd între ei dacă acceptă, iar vizualizarea Lume arată toate cercurile.
- **Interfață web**: turca, autentificarea activabilă din interfață și istoricul pe piesă sau pe ascultare.
- **Tune Client (aplicația desktop)**: găsește singură serverele din rețea și comandă serverul local.
- **Windows**: pornirea Tune odată cu Windows, propusă de programul de instalare.
- **Aplicația iOS**: CarPlay în 5G mai complet, bara laterală revine pe iPad.

## Îmbunătățiri

- **Tranziții**: ReplayGain „Auto” păstrează câștigul de album între două piese ale aceluiași album; AAC, Opus și piesele CUE se înlănțuie fără pauză.
- **Redare și ieșiri**: aceeași curbă de volum în AirPlay 1 și 2; un radio pornește mai repede pe un player de rețea.
- **Windows**: în modul exclusiv WASAPI, revenire la cealaltă familie de frecvențe în loc de oprirea piesei.
- **Devialet AIR**: volum în pași de 1 dB, până la 192 kHz.
- **DSD**: detectarea DSD-ului nativ este mai prudentă și se reevaluează.
- **Biblioteci mari**: Foldere se deschide fără așteptare, sarcinile de fundal nu mai blochează restul.
- **Servicii**: clientul de conectare Google propriu al Tune pentru YouTube; Spotify și TIDAL mai robuste în timpul redării.
- **Interfață web**: egalizator parametric mai precis și confirmare înainte de ștergere.
- **Aplicația iOS**: ecranul de blocare comandă zona chiar și oprită; fără DAC, cursorul reglează volumul iOS.
- **Actualizare și diagnosticare**: actualizarea înlocuiește și programele livrate cu Tune; rapoartele de erori ascund numele.

## Corecturi

- **Redare și ieșiri**: reluarea funcționează după o pauză care eliberase DAC-ul; „Repetă tot” nu mai confundă o oprire de moment cu sfârșitul unei piese; un DAC pornit după Tune este găsit pe Windows.
- **Zone**: un dispozitiv deja legat își numește zona; în OAAT, o reluare redeschide sesiunea în loc să rămână mută.
- **DLNA, UPnP și OpenHome**: fără Stop parazit pe Musicbook Source, sunet dezactivat stabil, Linn Klimax DS/2 recunoscut.
- **AirPlay**: ieșirea AirPlay 1 nu mai pornește la maximum după o repornire.
- **Calea semnalului**: nu mai anunță procesări care nu au loc.
- **Bibliotecă**: edițiile DSD și CD ale aceluiași album nu mai sunt îmbinate; scrierea DR nu mai blochează serverul.
- **Servicii**: favoritele Qobuz și TIDAL cu durata lor; secțiunile Spotify nu mai apar dublate.
- **Radiouri**: posturile FIP își recapătă logoul.
- **Pluginuri și crossfeed**: un plugin instalat acționează fără repornire; crossfeed-ul nu mai este blocat pe OAAT și nici în browser.
- **Interfață web**: grila derulează până la capăt; bannerul „serverul nu răspunde” dispare imediat ce acesta răspunde.

## Securitate

- Mai multe consolidări de securitate.
- Jurnalele și rapoartele de erori ascund acum datele de autentificare conținute în unele adrese de flux.
- Înlocuiește calea (Setări › dosare de muzică) verifică acum dosarele și nu mai este accesibil de pe alt dispozitiv fără autentificare.

## Schimbări de comportament

- **Tune Circle**: cu autentificarea, rezervat administratorilor; doar citire din ascultarea la distanță.
- **YouTube**: conturile conectate înaintea acestei versiuni trebuie reconectate o dată.
- **Radio la sfârșitul cozii**: din nou dezactivat implicit; bifați-l din nou în zonă dacă l-ați ales.
- **Egalizator**: oprirea egalizatorului îi păstrează preamplificarea până la „Restabilește nivelul”.
- **Multiroom**: grup gratuit până la 3 zone; peste, Premium. Perechile stereo rămân gratuite.
- **CUE cu INDEX 02**: începuturile pieselor corectate la următoarea scanare, istoric păstrat.
- **Tune OS**: parola temporară se afișează pe ecranul mașinii sau pe cardul de pornire, nu mai pe pagina de conectare; accesul de la distanță rămâne închis până când este schimbată.
- **Profiluri și istoric**: cu autentificarea, doar administratorul gestionează celelalte profiluri.
- **Înlocuiește calea**: fără autentificare, de pe mașina pe care rulează Tune.

## De verificat la dumneavoastră

Aceste modificări au trecut testele automate fără încercări pe echipamente reale. Spuneți-ne ce constatați:

- **Sendspin și Devialet AIR** cu o procesare activă.
- **Windows**: mini-playerul, modul exclusiv WASAPI, pornirea odată cu Windows.
- **AirPlay 1**: nivelul la pornire și între piese.
- **DSD spre un player de rețea** (Atoll ST200, Eversolo).
- **Linn Klimax DS/2** în OpenHome, **Tune Client pe macOS** și **CarPlay în 5G**.

## Probleme cunoscute

- **DSD în DoP pe ieșirea locală**: după un PCM atenuat, poate cânta la scară plină. Păstrați volumul amplificatorului scăzut.
- **UPnP după o schimbare de adresă**: o piesă poate să nu se redea; reindexați acel server.
- **Auditul de integritate**: câteva fișiere pot fi semnalate greșit; nu ștergeți nimic doar pe baza acestui verdict.
- **Windows ASIO controlat de un alt server**: nu mai este sunet după Pauză și apoi Redare; treceți la piesa următoare.
- **După actualizare**: dacă interfața nu se schimbă, forțați reîncărcarea (Cmd+Shift+R sau Ctrl+Shift+R).
