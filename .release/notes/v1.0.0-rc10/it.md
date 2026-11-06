Tune 1.0.0-rc10, decima release candidate di Tune 1.0.

## Novità

- **La mia collezione fisica**: un nuovo plugin confronta la tua collezione Discogs con la tua libreria e con i servizi di streaming, in una griglia di copertine memorizzate nella cache del server. Una voce dedicata compare nella barra laterale, subito dopo Tune Circle.
- **Il mio tema**: scegli tutti i colori dell'interfaccia, per profilo. Anche un plugin può fornire un tema, e puoi inviare il tuo a un contatto Tune Circle, che lo applica con un clic.
- **Crosstalk**: il plugin che attenua la diafonia tra i diffusori entra nel catalogo delle estensioni, con la sua schermata di impostazioni; sulla stessa zona disattiva il crossfeed e lo segnala.
- **Cartelle**: riproduci o aggiungi alla coda una cartella con tutte le sue sottocartelle, in ordine; il filtro Directory della Libreria scende nelle sottocartelle e propone di tornare all'intera libreria.
- **Radio**: una barra «Altri filtri» in Le mie radio e nell'Annuario consente di cercare per criteri; quattro nuovi generi arrivano nell'annuario.
- **Radio in riproduzione**: i flussi di una stazione sono ordinati per qualità; In riproduzione mostra il logo della stazione e un pulsante «Le mie radio»; il paese di ascolto si imposta in Impostazioni › Generali.
- **Etichette**: i loghi delle etichette discografiche vengono mostrati, condivisi da mozaiklabs.fr; lo stato del server segue l'avanzamento delle copertine degli album e dei loghi delle etichette.
- **Volume**: il cursore è graduato in dB, i limiti di comfort e di sicurezza compaiono a colori, e un limite di comfort superiore a quello di sicurezza viene rifiutato.
- **Oxygen e metadati**: modifica un campo su un intero risultato filtrato; escludi cartelle dall'arricchimento automatico dei metadati.
- **Tune Circle**: zoom su un'area del globo nella vista Mondo; le playlist di Tune Circle hanno il loro badge nei filtri delle Playlist; una playlist di un servizio si condivide dalla sua scheda.
- **Widget desktop**: la finestra mini riprende la vista mini dell'interfaccia web e guadagna una ricerca di artisti, Qobuz e TIDAL, con miniature e navigazione da tastiera.
- **Uscita AES67**: un'uscita sperimentale verso reti audio Dante e Ravenna compatibili AES67, disattivata per impostazione predefinita, si trova in Impostazioni › Sperimentale.

## Miglioramenti

- **Questo browser**: il brano successivo viene preparato prima della fine per un passaggio più fluido, e la zona «Questo browser» del server suona senza configurazione preliminare.
- **Riprendi l'ascolto**: la scheda indica l'album, la playlist o l'episodio ripreso, e un ascolto terminato esce dall'elenco.
- **Cronologia e brani più popolari**: i brani di un album o di una playlist aperti vanno dal più recente al più vecchio; i brani più popolari di un artista aggiungono quelli che ascolti di più, da tutte le fonti.
- **Libreria**: i filtri per DR e per anno originale sono molto più rapidi sulle librerie grandi; le colonne dell'elenco brani si ridimensionano e ogni schermata ne ricorda la larghezza; l'analisi acustica spiega il suo ritmo e mostra l'avanzamento.
- **Equalizzatore**: inserimento libero di frequenza e Q, convalidato a fine digitazione; l'eliminazione dei preset importati chiede conferma; la correzione ambientale e la calibrazione del DAC applicano il profilo di equalizzazione completo.
- **Playlist**: l'importazione .m3u, .m3u8 e .dpl si trova nel Gestore playlist e indica il numero reale di brani; Importa, Trasferisci e Confronta dicono perché falliscono.
- **Copertine**: la copertina incorporata viene letta in tutti i tag, prima il fronte; la ricerca online segue la rilettura del file; l'immagine rettangolare di una playlist Qobuz è mostrata per intero.
- **Messaggi**: gli ultimi messaggi di errore rimasti in francese sono tradotti nella lingua dell'interfaccia; un diffusore AirPlay 2 che rifiuta la connessione segnala un abbinamento mancante; un identificativo di applicazione rifiutato da Qobuz non viene più presentato come una password errata.
- **Licenza**: la schermata indica che il Premium proviene dal tuo account Mozaiklabs, senza chiedere una chiave.
- **Home**: «Altre versioni» non si blocca più in attesa dei servizi di streaming.
- **ASIO**: quando il supporto DSD non può essere verificato, Tune lo segnala e spiega cosa supporta l'uscita.

## Correzioni

- **Riproduzione**: passare tra PURE ed equalizzatore non riavvia più una zona ferma; deselezionare «Ripeti per impostazione predefinita» disattiva davvero la ripetizione della zona; le transizioni tra brani compatibili restano continue.
- **OpenHome**: pausa e stop rimuovono dal lettore il brano successivo preparato, e la posizione finale viene mantenuta durante il passaggio al successivo.
- **Radio**: con un paese di ascolto sconosciuto, il flusso scelto non viene più sostituito da un flusso a 96 kbit/s; una stazione aggiunta dalla console mantiene il collegamento con la sua scheda; un errore isolato non sospende più i metadati delle radio; «Ascoltate di recente» mostra la data corretta; la playlist Qobuz creata dai preferiti radio compare subito.
- **Libreria e profili**: i filtri dei preferiti e delle valutazioni seguono il profilo attivo; «I più ascoltati» non conta più un ascolto più volte; l'arricchimento non sostituisce più un genere esistente; i brani UPnP mantengono anno e generi; un identificativo MusicBrainz già assegnato non viene assegnato una seconda volta.
- **Collezioni e playlist**: un salvataggio rifiutato viene segnalato, una cartella difettosa non svuota più l'elenco, e tutte le pagine delle playlist vengono lette.
- **Plugin e account**: disinstallare un plugin nativo attivo lo disattiva in modo permanente; il primo collegamento a un account Mozaiklabs si associa al profilo unico anche senza indirizzo email.
- **Varie**: l'importazione delle playlist durante la scansione dice perché non importa; un errore di lettura delle impostazioni viene segnalato invece di tornare ai valori predefiniti; la sospensione del computer non viene più segnalata come un blocco; la vecchia interfaccia non sopravvive più a un aggiornamento del server; la barra dei VU-meter non straborda più tra 1280 e 1800 px; la modalità WASAPI resta visibile con l'impostazione predefinita di Windows; l'audio non viene più compresso all'invio.
- **Sicurezza**: correzioni di rafforzamento.
