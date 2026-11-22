Tune 1.0.0-rc12, a douăsprezecea versiune candidată a Tune 1.0. Mai multe ecrane primesc o prezentare nouă desenată de Levente, designer din comunitatea Tune.

## Noutăți

- **Interfață nouă**: Tune Circle reproiectat (Descoperă, Mesaje, Playlisturi partajate, Cercul meu); fișa albumului cu descriere, credite, detalii și o filă Versiuni cu numărul de catalog; pagina artistului cu un portret mare, cifre și file; Setări într-o coloană mai lizibilă; Starea serverului și tabloul de bord al ascultărilor, cu alegerea perioadei.
- **Acasă**: un widget nou „Rezumatul ascultărilor (prezentare nouă)”, de adăugat în Acasă › Editare, cu un panou Zone; miniaturi mai mari cu anul și genul.
- **Bibliotecă**: fixați filtrele dorite cu butonul Filtre; muzică clasică cu Dirijor, Ansamblu / orchestră și Operă; numărul de canale și filtrul Stereo; hartă și filtru după țara artistului; „Înlocuiește calea” unui folder de muzică; detectarea falsurilor Hi-Res, dezactivată implicit.
- **Fișa tehnică a unei piese**: spectrogramă și formă de undă pe canal, inclusiv pentru piesele Qobuz și TIDAL din bibliotecă.
- **Radio la sfârșitul cozii**: când coada se termină, Tune continuă cu piese similare, marcate „Radio”; se reglează zonă cu zonă.
- **Oprire programată**: muzica se oprește la ora aleasă; pe Tune OS, mașina se poate și stinge.
- **Playlisturi**: numărul de ascultări și ultima ascultare pe fiecare rând; crucea Elimină și „Șterge din acest playlist”; eliminarea unei piese din propriul playlist Qobuz.
- **Radio**: filtrele Fără pierderi, Calitate înaltă și Hi-Res pe un singur rând; o stație din director se modifică pentru dumneavoastră; un radio se adaugă într-un playlist.
- **Redare**: DSD nativ pe ieșirea locală Linux (ALSA); frecvență PCM fixă pe zonă; mod exclusiv reglat zonă cu zonă; zonele șterse se pot restaura; verificare AccurateRip reală a CD-urilor.
- **DLNA**: flux continuu între piese pentru rendererele care leagă prost două piese, dezactivat implicit; volumul principal al unui grup se reglează de la oricare membru.
- **Streaming**: alegerea țării catalogului TIDAL; noutățile Qobuz identice cu cele din playerul Qobuz; Spotify cu un asistent de conectare, iar Spotify Connect cântă în zona aleasă.
- **Calea semnalului**: un clic pe o procesare îi deschide setările; redarea grupată arată fiecare ieșire.
- **Setări și interfață**: căutare după setare, dimensiunea textului, bară de progres în formă de undă, un singur câmp de căutare pe toate ecranele, dictare vocală în asistent.

## Îmbunătățiri

- **Viteză**: colecțiile inteligente și salturile alfabetice mult mai rapide pe o bibliotecă mare; listele mari nu mai încetinesc serverul.
- **Memorie**: Tune eliberează memoria disponibilizată în timpul unei ascultări lungi; memoria transcodării este plafonată.
- **Pornirea sunetului**: YouTube pornește mai repede; TIDAL nu mai așteaptă descărcarea completă a piesei pe o ieșire locală sau OAAT.
- **Statistici**: un singur total de piese și albume pe toate ecranele, o singură intrare pe codec, căutare în tot clasamentul.
- **Tune Circle**: emoji în Mesaje; când ascultarea la un contact este închisă, Tune caută aceeași piesă în sursele dumneavoastră.
- **Aplicații mobile**: CarPlay redă direct pe telefon, cu Colecții și o filă Radio; Android primește catalogul TIDAL, istoricul radiourilor și podcasturilor și un widget Radiouri favorite.
- **Windows**: un banner avertizează când firewallul filtrează Tune; insignă WASAPI sau ASIO pe zonele locale.
- **Docker**: asistentul de partajare SMB funcționează în imagine; unitatea CD poate fi folosită în container.
- **Traduceri**: forme de plural corecte în cele 15 limbi.
- **Dependențe** actualizate, printre care SQLite 3.54 integrat.

## Corecturi

- **Interfața web**: gata cu ecranul negru după o actualizare a interfeței.
- **Mut**: o zonă pe mut rămâne fără sunet când pornește o piesă nouă; o alarmă sau o limită de volum nu mai anulează modul mut.
- **Redare**: un fișier deteriorat nu mai blochează zona, Tune trece mai departe; un episod de podcast terminat nu se mai redeschide la final.
- **AirPlay 2**: după o pauză, redarea reia de unde se oprise și pornește la volumul zonei, niciodată la putere maximă.
- **DLNA și grupuri**: redarea reia după o eroare 701; piesa următoare este pregătită din timp; un dispozitiv care nu răspunde nu mai blochează celelalte zone; Sonos primește numele modelului său.
- **HQPlayer, Devialet și Diretta**: poziția și starea sunt urmărite, reconectare automată; amplificatorul Devialet răspunde din nou; nu mai apare un fals cursor de volum pe Diretta.
- **OAAT**: DSD nativ afișat și bit-perfect; DSD1024 recunoscut, cu revenire la PCM pe un receptor limitat.
- **Radiouri**: nu mai există stații dublate; un radio refuzat în țara dumneavoastră trece la fluxul următor.
- **Spotify, Qobuz și TIDAL**: playlisturile și favoritele Spotify urmează noul API; un refuz Qobuz nu mai este prezentat drept date de autentificare greșite; preîncărcarea TIDAL nu mai întârzie redarea.
- **Bibliotecă**: ascultările se numără pe piesă; o cratimă nu mai creează un fals artist; discurile dintr-un set sunt recunoscute mai bine; o piesă fără an îl preia pe cel al albumului.
- **Windows**: un DAC absent nu mai trimite sunetul în difuzoarele PC-ului; supravegherea folderelor acoperă partajările de rețea.
- **PostgreSQL 18**: favoritele UPnP funcționează din nou.

## Securitate

- Administrarea Tune de pe o altă mașină din rețea cere acum autentificare (Tune OS: până la noua imagine).
- Webhookurile nu mai pot viza chiar mașina Tune.
- Configurarea Home Assistant și Hue cere drepturi de administrare sau chiar mașina Tune; un Home Assistant instalat pe aceeași mașină cu Tune nu mai este acceptat.

## Probleme cunoscute

- **DSD în DoP pe ieșirea locală**: poate cânta la scară plină după un PCM atenuat, fără să țină cont de volum. Păstrați volumul amplificatorului scăzut când treceți de la o piesă PCM la o piesă DSD în DoP (versiunea următoare).
- **Auditul de integritate**: multe fișiere FLAC pot fi semnalate greșit drept corupte; nu ștergeți niciun fișier doar pe baza acestui verdict.
- **Windows ASIO controlat de un alt server**: nu mai este sunet după Pauză și apoi Redare; treceți la piesa următoare.
- **După actualizare**: dacă interfața nu se schimbă, forțați reîncărcarea (Cmd+Shift+R sau Ctrl+Shift+R). Prima pornire durează mai mult, cât timp se creează două indexuri.
