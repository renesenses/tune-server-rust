Tune 1.0.0-rc14 include tot conținutul versiunilor preliminare rc13.90 și rc13.91; aceste note descriu ce s-a schimbat de la rc13.91. Mulțumim testerilor: multe dintre aceste puncte vin din observațiile dumneavoastră.

## Noutăți

- **TESS, presetări Salvează și Reîncarcă**: imediat ce o setare se abate de la presetarea aleasă, TESS afișează „modificat”. **Salvează** suprascrie presetarea originală, **Reîncarcă** revine la ea. O presetare livrată cu Tune, precum „Pure”, nu poate fi suprascrisă: „Salvează ca…” creează o copie.
- **TESS, presetare DSD aplicată imediat**: o presetare care schimbă ținta DSD (de exemplu DSD128 → DSD256), încărcată în modul „aplicare imediată”, repornește piesa de la poziția sa, în loc să aștepte piesa următoare. Supraeșantionarea este o funcție Premium.
- **Rezervă (headroom)**: Automat sau manual, de la −12 la 0 dB, în pași de 0,5 dB. Panoul afișează valoarea aplicată efectiv sau spune de ce nu este aplicată.
- **Rezerva la supraeșantionarea în DSD**: rezerva manuală nu mai este aplicată de două ori (−12 dB dădea −24 dB; acum este −15 dB, din care −3 dB pentru modulatorul DSD, afișați pe o linie separată).
- **Măsurarea camerei ghidată spre 75 dB(C)**: cu un microfon calibrat, ecranul afișează nivelul măsurat, abaterea față de 75 dB(C) și o indicație de volum. Regula este prudentă, deoarece conversia nu a fost încă verificată cu un calibrator: Tune nu recomandă niciodată depășirea a 80 dB(C) estimați și întrerupe semnalul la 90 dB SPL. Corecția măsurată cu microfonul este Premium; cea calculată fără microfon rămâne gratuită.
- **Protecția auzului**: debifarea unui egalizator, trecerea în PURE sau Bypass all, ori eliminarea unei convoluții, a unui crossfeed sau a unei combinări mono nu mai face nivelul să urce brusc: acesta revine la cel anterior printr-o tranziție de aproximativ o jumătate de secundă. Dacă ați mărit volumul între timp, indicatorul „nivel redus” se aprinde, iar „Restabilește nivelul” eliberează surplusul. Același lucru este valabil și pe ieșirile de rețea (UPnP, OAAT).
- **O cale a semnalului fidelă**:
  - pe o ieșire de rețea, urmează ordinea reală a fluxului, până la volumul playerului;
  - etapa unui filtru de răspuns la impuls se numește „Convoluție”;
  - filtrul păstrează numele fișierului .wav sau .zip importat;
  - după o schimbare a țintei DSD, afișează rata DSD redată efectiv și o anunță pe următoarea;
  - fereastra devine verde: portocaliul este rezervat alertelor reale.
- **Convoluție**: panoul afișează curba filtrului încărcat, canal cu canal, și faza la cerere.
- **Bypass all**: pictograma TESS din bara de redare este estompată când TESS este ocolit, iar pagina TESS afișează „TESS ocolit”.
- **Pagina TESS armonizată**: accent verde TESS, zona curentă amintită lângă insignă, panoul Headroom estompat pe o ieșire de rețea și măsurare „Fără microfon (estimare)” sau „Cu microfon (recomandat)”.
- **Ieșire de rețea**: cardul Supraeșantionare spune că supraeșantionarea nu se aplică acolo.
- **Interfață armonizată**: fonturi încorporate, disponibile și offline; Bibliotecă, Live Radio, Servere media și Concerte urmează noua identitate vizuală.

## Corecturi

- **Radio către un Sonos** (și orice player de rețea cu rată plafonată): un post de radio la 192 kHz este servit la rata pe care playerul o acceptă. Înainte, playerul se oprea imediat, în buclă.
- **768 kHz**: lista „Frecvență max.” a unei zone oferă 768 kHz și 1.536 kHz.
- **Bibliotecă**: grila albumelor nu se mai blochează în timpul derulării unei colecții mari.
- **Redare în fila browserului**: deplasarea în piesă mută cu adevărat redarea, chiar și de pe un alt dispozitiv.
- **Ajutor**: bannerul suportului prioritar nu mai apare pentru un cont Premium.
- **Licență**: data expirării este citită corect, iar Setări › Licență spune de ce nivelul este gratuit.
- **Convoluție**: un filtru care amplifică anumite frecvențe nu mai saturează.
- **Podcasturi lungi**: un episod a cărui gazdă întrerupe regulat conexiunea nu mai este oprit.
- **Pachet Debian într-un container LXC** (Proxmox): serviciul pornește chiar și fără spații de montare private.
- **Prima pornire**: asistentul nu mai numește „activate” serviciile fără cont și salvează folderul de muzică ales. „Radiourile mele” goale deschid directorul de posturi.

## Securitate și conturi

- **Sesiune reînnoită**: cu „Rămâi conectat” (debifat implicit), sesiunea browserului se reînnoiește cât timp este folosită, până la 90 de zile. Altfel, 24 h, ca înainte. Meniul avatarului oferă „Deconectare de la acest server”.
- **Recuperarea accesului de administrator**: rulați `tune-server --reset-admin` (sau `--reinitialiser-admin`) **pe mașina serverului**. Codul de unică folosință obținut, valabil 15 minute, se introduce în „Ai uitat parola?” de pe această mașină; fără browser, adăugați `--password-stdin`. Sesiunile deschise sunt închise. Ghidul de instalare (secțiunea 9) indică comanda pentru fiecare sistem.
- **Profilul `admin`**: profilul de administrator creat implicit se numește acum `admin`; un nume ales de dumneavoastră este păstrat, iar `default` rămâne acceptat.
- **Parola de administrator de la prima configurare**: pe o instalare nouă, asistentul vă cere să o creați.
- **Server fără autentificare**: administrarea este acceptată din rețeaua locală, nu doar de pe mașina serverului. **Atenție**: fără autentificare, dispozitivele din rețeaua locală pot administra serverul; activați autentificarea dacă rețeaua dumneavoastră nu este de încredere. Setarea „Administrare doar de pe această mașină” restabilește regula anterioară.

## Telecomenzi

- Pe un server protejat, aplicațiile care nu sunt încă actualizate cer din nou parola la fiecare 24 h, ca înainte.

## De verificat la dumneavoastră

- **„Bypass all” într-o presetare**: o presetare care schimbă în același timp Bypass all și ținta DSD, în modul „aplicare imediată”, repornește piesa de la poziția sa. Verificat doar pe bancul de test: spuneți-ne dacă reluarea este curată.
