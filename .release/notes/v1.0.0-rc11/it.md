Tune 1.0.0-rc11, undicesima release candidate di Tune 1.0.

## Novità

- **Correzione della stanza**: un plugin, disattivato di default, misura la stanza con un microfono (sweep, risposta all'impulso, RT60) o ne simula i modi dei bassi senza microfono, poi propone un target e un equalizzatore automatico che si limita ad attenuare, con un profilo per zona; senza Premium, la convoluzione con la risposta all'impulso si spegne in dissolvenza.
- **Controllo parentale**: regole per profilo in Impostazioni › Profili, nascondere i contenuti espliciti, fasce orarie e budget di ascolto.
- **Sovracampionamento**: un plugin Premium di sovracampionamento di alta qualità si collega all'uscita locale, con la sua impostazione per zona.
- **Query avanzata**: interrogate la libreria con un'espressione, la stessa che definisce le raccolte intelligenti.
- **Una scheda per album**: le versioni di un album sono raggruppate e si scelgono nella scheda; compaiono nuovi filtri Qualità DSD e Multicanale e il numero di canali.
- **Note e statistiche**: note personali in Markdown e statistiche della libreria per ripartizione.
- **Catalogo e spettrogramma**: esportate la libreria come catalogo HTML stampabile, con i filtri attuali; mostrate lo spettrogramma di un brano dal suo menu; un pulsante interrompe l'analisi del DR tra due brani.
- **Mappe del mondo**: mappa dei paesi e filtro per paese nella Libreria; il globo traccia confini, coste e grandi fiumi e nomina il paese al passaggio del cursore.
- **Podcast**: aggiungete un podcast con l'indirizzo del suo feed RSS.
- **Mix TIDAL**: la riga «I tuoi mix» in Streaming › TIDAL mostra e riproduce i mix personali dell'account.
- **Importazione dell'equalizzatore**: importate un file di equalizzatore (AutoEq, Equalizer APO, OPRA) con un'anteprima delle bande prima di applicarlo.
- **Biografie**: link ad artisti e album di Tune e alla fonte; biografie e note d'album completate su richiesta da Qobuz, senza sovrascrivere nulla e in modo reversibile.
- **Playlist**: sostituite sul posto un brano di servizio non disponibile e riparate la playlist.
- **Tune Circle**: scrivete a un intero cerchio in Messaggi; i cerchi generali e il consenso del thread passano dal relay.
- **Collezione fisica**: copertina di ogni disco, ricerca sui servizi di streaming, navigazione per cartelle, generi e stili.

## Miglioramenti

- **Radio**: filtro per qualità minima del flusso in «Altri filtri», «Le mie radio» separato da «Ascoltate di recente», flusso predefinito sempre riproducibile nel paese di ascolto.
- **Tune tested**: Tune propone di aggiungere un dispositivo dopo un'ora di ascolto senza errori, da un pulsante dedicato; un dispositivo già in catalogo aggiunge una famiglia.
- **Libreria**: impostazione «Libreria estesa ai preferiti dei servizi», attiva di default; ricerca per inizio parola più veloce; la ricerca della griglia richiede tutte le parole; periodo Da / A sulla linea degli anni; scorrimento dei brani più fluido.
- **Elenchi di brani**: l'ordine delle colonne si cambia e ogni schermata lo ricorda; la schermata di una playlist ottiene copertine, dimensione delle miniature e intestazione compatta; badge di origine nel Gestore playlist.
- **Home e ripresa**: scegliete i pannelli del riepilogo degli ascolti; «Riprendi l'ascolto» mostra un sottotitolo per tipo, riprende un podcast con titolo e copertina (la ripresa alla posizione salvata su una zona DLNA arriva nella prossima versione), e senza duplicati.
- **Cronologia**: un ascolto è valido solo dopo 30 s o metà del brano; «Riproduci» dopo Stop mantiene l'album o la playlist in corso; gli episodi di podcast e le radio portano il nome della stazione.
- **Statistiche di ascolto**: gli ascolti importati (Roon, Plex, m3u) contano tra i più ascoltati; le liste intelligenti e la Cronologia contano come la Libreria; un ascolto Qobuz non conta più per l'album locale omonimo.
- **Crosstalk ed equalizzatore**: curva separazione / colorazione e risposta dei diffusori nella schermata di regolazione, separazione obiettivo da 1 dB; grafico parametrico fisso di default (Modifica / Fatto) e un colore per canale.
- **Estensioni**: un pannello «Stato dei plugin» per l'amministratore; una scheda del catalogo e il suo plugin locale sono ora un'unica estensione.
- **Notifiche**: gli errori persistono, si mettono in pausa al passaggio del cursore e si raggruppano; gli ultimi messaggi d'errore fissi sono tradotti; il testo grezzo dell'eccezione è ripiegato in «Dettagli».
- **Analisi in background**: l'avanzamento è mostrato, la priorità è bassa e il default prudente dopo la scansione; l'arricchimento riprende da dove era e riprova dopo un 503; «Avvia passata» dei loghi delle etichette agisce anche in pausa.
- **Supporto e accesso remoto**: la segnalazione di bug allega i log; il link, il codice QR e il pulsante copia tornano in Impostazioni › Accesso remoto.
- **Riproduzione**: i motivi di transcodifica sono nominati e il FLAC è servito così com'è quando nulla si applica; Avanti e Indietro saltano i brani rifiutati dal servizio.

## Correzioni

- **AirPlay 2**: il pulsante «Pair» abbina tramite il componente AirPlay 2 o ne nomina la causa; l'attesa del PIN non blocca più la riproduzione e chiuderne la finestra la annulla; la sessione si riapre dopo una pausa; la posizione avanza e una pausa non va più in loop; il componente esiste per Raspberry Pi a 64 bit.
- **DLNA e OpenHome**: brani DSD senza interruzioni, «avanti» più rapido, una pausa fatta sul dispositivo è rispettata, l'uscita della macchina non appare più doppia, volume OpenHome senza attesa, l'elenco caricato da Kazoo è conservato, Play più rapido su TIDAL CD.
- **Volume**: una nuova uscita locale non parte più a fondo scala; togliendo il silenziamento, il cursore torna al volume precedente; ogni posizione del cursore dà un livello distinto; il volume WASAPI e ASIO si sente subito; Tune segue il dispositivo sotto il tetto di sicurezza.
- **Windows**: il collegamento si apre senza finestra di console; un DoP che l'uscita non può portare parte come PCM; il rilevamento di rete supera il firewall; una temperatura illeggibile non frena più le analisi; l'aggiornamento avvia lo script di sostituzione all'uscita.
- **Aggiornamenti e licenza**: su Unix, il nuovo binario e l'interfaccia web sono validati prima di sostituire i vecchi, e un errore è esplicito; rimuovere la licenza libera anche la macchina sul sito.
- **Libreria**: foto e video di un server UPnP non entrano più; un cofanetto FLAC + CUE forma un solo album; le edizioni DSD e CD restano due album; una compilation fatta a mano non viene più spezzata; una playlist eliminata non viene più ricreata alla scansione; una rilettura senza tag artista non sposta più l'album in «Unknown Artist»; aggiungere una cartella scansiona solo quella cartella.
- **Copertine e analisi**: come ultima risorsa, l'immagine più grande della cartella; analisi dei falsi Hi-Res su tutta la durata del brano; MP3 senza interruzioni; l'audit di integrità non dichiara più troncati FLAC intatti.
- **Servizi di streaming**: la ricerca Spotify restituisce risultati, un rifiuto non è più muto e i preferiti superano i 50 brani; TIDAL › Preferiti non blocca più Tune; un account Qobuz senza preferiti restituisce subito la lista vuota; un brano convertito parte in Safari.
- **Rete e archiviazione**: il rilevamento riprende quando la rete arriva dopo Tune; un mount SMB che fallisce dice perché, anche sotto Docker; il backup riesce su PostgreSQL; l'estrazione di CD rifiuta una destinazione piena o non scrivibile; Tune lascia le porte Squeezebox a un server Lyrion sulla stessa macchina.
- **Schermate**: la schermata Concerti si apre senza licenza; «Separa tutto» ricompare; In riproduzione segue il cambio di album; una sessione di streaming scaduta mantiene la scheda «Riconnetti»; Nuova sveglia resta visibile.
- **Raspberry Pi 5**: la ventola dell'Active Cooler torna a girare (nuova immagine di Tune OS).
- **Sicurezza**: correzioni di rafforzamento; su un server senza autenticazione, l'esportazione della configurazione con i segreti in chiaro viene rifiutata (usate il backup cifrato con una passphrase).

## Problemi noti

- **Silenziamento su una zona DLNA**: una zona silenziata può ripartire con il suono quando si avvia un nuovo brano. Fino alla soluzione, abbassate il volume invece di silenziare (prossima versione).
- **Ripresa di un episodio di podcast**: su una zona DLNA, un episodio ripreso riparte dall'inizio (prossima versione).
- **Tune Circle**: le cerchie di cui siete membri, o a cui siete invitati, non compaiono ancora nell'elenco come quelle che avete creato (prossima versione).
