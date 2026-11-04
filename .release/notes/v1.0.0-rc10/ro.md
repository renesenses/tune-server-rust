Tune 1.0.0-rc10, a zecea versiune candidată a Tune 1.0.

## Noutăți

- **Colecția mea fizică**: un nou plugin compară colecția dvs. Discogs cu biblioteca și cu serviciile de streaming, într-o grilă de coperți păstrate în memoria cache a serverului. O intrare dedicată apare în bara laterală, imediat după Tune Circle.
- **Tema mea**: alegeți toate culorile interfeței, pe profil. Și un plugin poate furniza o temă, iar pe a dvs. o puteți trimite unui contact Tune Circle, care o aplică dintr-un clic.
- **Crosstalk**: pluginul care atenuează diafonia dintre boxe intră în catalogul de extensii, cu ecranul său de setări; în aceeași zonă dezactivează crossfeed-ul și o semnalează.
- **Dosare**: redați sau adăugați în coadă un dosar cu toate subdosarele sale, în ordine; filtrul Director din Bibliotecă coboară în subdosare și propune revenirea la întreaga bibliotecă.
- **Radio**: o bară „Mai multe filtre” în Radiourile mele și în Director permite căutarea după criterii; patru genuri noi apar în director.
- **Radio în redare**: fluxurile unui post sunt ordonate după calitate; Redare curentă afișează logoul postului și un buton „Radiourile mele”; țara de ascultare se setează în Setări › General.
- **Case de discuri**: logourile caselor de discuri sunt afișate, puse la dispoziție de mozaiklabs.fr; starea serverului urmărește progresul coperților de album și al logourilor.
- **Volum**: cursorul este gradat în dB, limitele de confort și de siguranță apar colorate, iar o limită de confort peste limita de siguranță este refuzată.
- **Oxygen și metadate**: modificați un câmp pe un întreg rezultat filtrat; excludeți dosare de la îmbogățirea automată a metadatelor.
- **Tune Circle**: zoom pe o zonă a globului în vizualizarea Lume; playlisturile Tune Circle au propria etichetă în filtrele Playlisturi; un playlist al unui serviciu se partajează din fișa sa.
- **Widget de birou**: fereastra mini preia vizualizarea mini a interfeței web și primește o căutare de artiști, Qobuz și TIDAL, cu miniaturi și navigare de la tastatură.
- **Ieșire AES67**: o ieșire experimentală către rețele audio Dante și Ravenna compatibile AES67, dezactivată implicit, se află în Setări › Experimental.

## Îmbunătățiri

- **Acest browser**: piesa următoare este pregătită înainte de final pentru o trecere mai fluidă, iar zona „Acest browser” a serverului redă fără configurare prealabilă.
- **Reluarea ascultării**: cardul numește albumul, playlistul sau episodul reluat, iar o ascultare terminată iese din listă.
- **Istoric și piese de top**: piesele unui album sau playlist deschis merg de la cea mai recentă la cea mai veche; piesele de top ale unui artist le adaugă pe cele pe care le ascultați cel mai mult, din toate sursele.
- **Bibliotecă**: filtrele după DR și după anul original sunt mult mai rapide în bibliotecile mari; coloanele listei de piese se pot redimensiona și fiecare ecran le reține lățimea; analiza acustică își explică ritmul și își arată progresul.
- **Egalizator**: introducere liberă a frecvenței și a factorului Q, validată la finalul introducerii; ștergerea presetărilor importate cere confirmare; corecția camerei și calibrarea DAC-ului aplică profilul complet de egalizare.
- **Playlisturi**: importul .m3u, .m3u8 și .dpl se află în Managerul de playlisturi și anunță numărul real de piese; Importă, Transferă și Compară spun de ce eșuează.
- **Coperți**: coperta încorporată este citită din toate etichetele, fața întâi; căutarea online urmează recitirii fișierului; imaginea dreptunghiulară a unui playlist Qobuz este afișată întreagă.
- **Mesaje**: ultimele mesaje de eroare rămase în franceză sunt traduse în limba interfeței; o boxă AirPlay 2 care refuză conexiunea semnalează lipsa împerecherii; un identificator de aplicație refuzat de Qobuz nu mai este prezentat drept parolă greșită.
- **Licență**: ecranul arată că Premium provine din contul dvs. Mozaiklabs, fără a cere o cheie.
- **Acasă**: „Alte versiuni” nu se mai blochează așteptând serviciile de streaming.
- **ASIO**: când suportul DSD nu poate fi verificat, Tune o semnalează și explică ce permite ieșirea.

## Corecturi

- **Redare**: comutarea între PURE și egalizator nu mai repornește o zonă oprită; debifarea „Repetare implicită” oprește cu adevărat repetarea zonei; trecerile între piese compatibile rămân continue.
- **OpenHome**: pauza și oprirea scot din player piesa următoare pregătită, iar poziția de final este păstrată în timpul trecerii la următoarea.
- **Radio**: cu o țară de ascultare necunoscută, fluxul ales nu mai este înlocuit cu unul de 96 kbit/s; un post adăugat din consolă își păstrează legătura cu fișa sa; un eșec izolat nu mai suspendă metadatele radio; „Ascultate recent” afișează data corectă; playlistul Qobuz creat din favoritele radio apare imediat.
- **Bibliotecă și profiluri**: filtrele de favorite și de note urmează profilul activ; „Cele mai ascultate” nu mai numără o ascultare de mai multe ori; îmbogățirea nu mai înlocuiește un gen existent; piesele UPnP își păstrează anul și genurile; un identificator MusicBrainz deja atribuit nu mai este atribuit a doua oară.
- **Colecții și playlisturi**: o salvare refuzată este semnalată, un dosar defect nu mai golește lista și toate paginile de playlisturi sunt citite.
- **Pluginuri și cont**: dezinstalarea unui plugin nativ activ îl dezactivează definitiv; prima asociere cu un cont Mozaiklabs se leagă de profilul unic chiar și fără adresă de e-mail.
- **Diverse**: importul playlisturilor la scanare spune de ce nu importă; un eșec la citirea setărilor este semnalat în loc să se revină la valorile implicite; trecerea calculatorului în repaus nu mai este semnalată drept blocare; vechea interfață nu mai supraviețuiește unei actualizări a serverului; bara cu VU-metre nu mai depășește ecranul între 1280 și 1800 px; modul WASAPI rămâne vizibil cu setarea implicită din Windows; sunetul nu mai este comprimat la trimitere.
- **Securitate**: corecturi de consolidare.
