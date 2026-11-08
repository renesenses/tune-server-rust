Tune 1.0.0-rc11, a unsprezecea versiune candidată a Tune 1.0.

## Noutăți

- **Corecția camerei**: un plugin, dezactivat implicit, măsoară camera cu un microfon (baleiaj, răspuns la impuls, RT60) sau îi simulează modurile de bas fără microfon, apoi propune o țintă și un egalizator automat care doar atenuează, cu un profil pe zonă; fără Premium, convoluția cu răspunsul la impuls se stinge treptat.
- **Control parental**: reguli pe profil în Setări › Profiluri, ascunderea conținutului explicit, intervale orare și buget de ascultare.
- **Supraeșantionare**: un plugin Premium de supraeșantionare de înaltă calitate se conectează la ieșirea locală, cu setare pe zonă.
- **Interogare avansată**: interogați biblioteca cu o expresie, aceeași care definește colecțiile inteligente.
- **O fișă pe album**: versiunile unui album sunt grupate și se aleg în fișă; apar filtre noi pentru calitatea DSD și multicanal, precum și numărul de canale.
- **Note și statistici**: note personale în Markdown și statistici ale bibliotecii pe repartiții.
- **Catalog și spectrogramă**: exportați biblioteca ca un catalog HTML imprimabil, cu filtrele curente; afișați spectrograma unei piese din meniul ei; un buton oprește analiza DR între două piese.
- **Hărți ale lumii**: hartă a țărilor și filtru pe țară în Bibliotecă; globul trasează granițele, coastele și marile fluvii și numește țara la trecerea cursorului.
- **Podcasturi**: adăugați un podcast cu adresa fluxului său RSS.
- **Mixuri TIDAL**: rândul „Mixurile tale” din Streaming › TIDAL afișează și redă mixurile personale ale contului.
- **Import de egalizator**: importați un fișier de egalizator (AutoEq, Equalizer APO, OPRA) cu o previzualizare a benzilor înainte de aplicare.
- **Biografii**: linkuri către artiștii și albumele din Tune și către sursă; biografiile și notele de album se completează la cerere din Qobuz, fără a suprascrie nimic și reversibil.
- **Playlisturi**: înlocuiți pe loc o piesă de serviciu indisponibilă și reparați playlistul.
- **Tune Circle**: scrieți unui cerc întreg în Mesaje; cercurile generale și consimțământul firului trec prin releu.
- **Colecție fizică**: coperta fiecărui disc, căutare în serviciile de streaming, navigare pe foldere, genuri și stiluri.

## Îmbunătățiri

- **Radiouri**: filtru după calitatea minimă a fluxului în „Mai multe filtre”, „Radiourile mele” separate de „Ascultate recent”, flux implicit mereu redabil în țara de ascultare.
- **Tune tested**: Tune propune adăugarea unui dispozitiv după o oră de ascultare fără erori, dintr-un buton dedicat; un dispozitiv deja din catalog adaugă o gospodărie.
- **Bibliotecă**: setarea „Bibliotecă extinsă la favoritele serviciilor”, activată implicit; căutare după începutul cuvântului mai rapidă; căutarea în grilă cere toate cuvintele; perioadă De la / Până la pe axa anilor; derulare mai fluidă a pieselor.
- **Liste de piese**: ordinea coloanelor se poate schimba și fiecare ecran o ține minte; ecranul unui playlist primește coperte, dimensiunea miniaturilor și un antet compact; insigne de sursă în Managerul de playlisturi.
- **Acasă și reluare**: alegeți panourile rezumatului ascultărilor; „Reia ascultarea” arată un subtitlu pe tip, reia un podcast cu titlu, copertă și poziție, și fără dubluri.
- **Istoric**: o ascultare se validează abia după 30 s sau jumătate din piesă; „Redare” după Stop păstrează albumul sau playlistul curent; episoadele de podcast și radiourile poartă numele stației.
- **Statistici de ascultare**: ascultările importate (Roon, Plex, m3u) contează la cele mai ascultate; listele inteligente și Istoricul contează ca Biblioteca; o ascultare Qobuz nu mai contează pentru albumul local cu același nume.
- **Crosstalk și egalizator**: curbă separare / colorare și răspunsul boxelor în ecranul de reglaj, separare țintă de la 1 dB; grafic parametric fixat implicit (Editare / Gata) și o culoare pe canal.
- **Extensii**: un panou „Starea pluginurilor” pentru administrator; o fișă din catalog și pluginul ei local sunt acum o singură extensie.
- **Notificări**: erorile persistă, se opresc la trecerea cursorului și se grupează; ultimele mesaje de eroare scrise fix sunt traduse; textul brut al excepției este strâns în „Detalii”.
- **Analize de fundal**: se afișează progresul, prioritatea este scăzută, iar valoarea implicită prudentă după scanare; îmbogățirea reia de unde a rămas și reîncearcă după un 503; „Pornește trecerea” pentru logourile casetelor de discuri acționează și în pauză.
- **Suport și acces la distanță**: raportul de eroare atașează jurnale; linkul, codul QR și butonul de copiere revin în Setări › Acces la distanță.
- **Redare**: motivele transcodării sunt numite, iar FLAC este servit ca atare când nu se aplică nimic; Următoarea și Precedenta sar piesele refuzate de serviciu.

## Corecturi

- **AirPlay 2**: butonul „Pair” asociază prin componenta AirPlay 2 sau numește cauza; așteptarea PIN-ului nu mai blochează redarea, iar închiderea ferestrei o anulează; sesiunea se redeschide după o pauză; poziția avansează și o pauză nu mai intră în buclă; componenta există pentru Raspberry Pi pe 64 de biți.
- **DLNA și OpenHome**: piese DSD fără pauze, „următoarea” mai rapidă, o pauză făcută pe dispozitiv este respectată, ieșirea mașinii nu mai apare dublat, volum OpenHome fără așteptare, lista încărcată de Kazoo este păstrată, Play mai rapid pe TIDAL CD.
- **Volum**: o ieșire locală nouă nu mai pornește la scară plină; mut nu mai lasă să treacă un volum recitit; fiecare poziție a cursorului dă un nivel distinct; volumul WASAPI și ASIO se aude imediat; Tune urmărește dispozitivul sub plafonul de siguranță.
- **Windows**: scurtătura se deschide fără fereastră de consolă; un DoP pe care ieșirea nu îl poate purta pleacă ca PCM; descoperirea rețelei trece de firewall; o temperatură ilizibilă nu mai frânează analizele; actualizarea pornește scriptul de comutare la ieșire.
- **Actualizări și licență**: pe Unix, noul binar și interfața web sunt validate înainte de a le înlocui pe cele vechi, iar un eșec este explicit; retragerea licenței eliberează și mașina pe site.
- **Bibliotecă**: fotografiile și videoclipurile unui server UPnP nu mai intră; un set FLAC + CUE formează un singur album; edițiile DSD și CD rămân două albume; o compilație făcută manual nu mai este fragmentată; un playlist șters nu mai este recreat la scanare; o recitire fără etichetă de artist nu mai mută albumul sub „Unknown Artist”; adăugarea unui folder scanează doar acel folder.
- **Coperte și analize**: ca ultimă soluție, cea mai mare imagine din folder; analiza falsurilor Hi-Res pe toată durata piesei; MP3 fără pauze; auditul de integritate nu mai declară trunchiate fișiere FLAC intacte.
- **Servicii de streaming**: căutarea Spotify returnează rezultate, un refuz nu mai este mut, iar favoritele depășesc 50 de piese; TIDAL › Favorite nu mai blochează Tune; un cont Qobuz fără favorite întoarce rapid lista goală; o piesă convertită pornește în Safari.
- **Rețea și stocare**: descoperirea reia când rețeaua apare după Tune; o montare SMB care eșuează spune de ce, inclusiv sub Docker; copia de siguranță reușește pe PostgreSQL; extragerea de CD refuză o destinație plină sau neinscriptibilă; Tune lasă porturile Squeezebox unui server Lyrion de pe aceeași mașină.
- **Ecrane**: ecranul Concerte se deschide fără licență; „Degrupează tot” reapare; Acum se redă urmărește schimbarea albumului; o sesiune de streaming expirată își păstrează fila „Reconectează”; Alarmă nouă rămâne vizibilă.
- **Raspberry Pi 5**: ventilatorul Active Cooler pornește din nou (imagine Tune OS nouă).
- **Securitate**: corecturi de consolidare.
