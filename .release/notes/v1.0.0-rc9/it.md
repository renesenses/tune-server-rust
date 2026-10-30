Tune 1.0.0-rc9, la nona release candidate di Tune 1.0.

## Novità

- **Tune Circle**: messaggistica privata tra contatti, con schede musicali da ascoltare con un clic, eliminazione dei propri messaggi e segnalazione. I messaggi non sono cifrati end-to-end e possono essere letti da Mozaiklabs dopo una segnalazione.
- **Tune Circle**: «Scrivi» dall'elenco dei contatti apre direttamente la conversazione; Invio invia il messaggio; layout adatto al telefono.
- **Tune Circle**: ritrova le cerchie a cui sei stato invitato e condividi una playlist; un badge e un elenco raccolgono le notifiche, regolabili per tipo.
- **Tune Circle**: la vista Mondo mostra la comunità in forma anonima, per paese e per zone di circa 110 km. Puoi togliere la tua presenza dalla mappa pubblica.
- **Radio**: la ricerca generale trova le tue stazioni e quelle dell'elenco, con più parole in qualsiasi ordine. Una scheda mostra la stazione, i generi, il sito e i flussi, e puoi scegliere il flusso preferito.
- **Mappe e podcast**: esplora le stazioni per genere, paese e rete, con una vista a elenco; sfoglia podcast di più paesi.
- **Ripresa della riproduzione**: la ripresa ritrova l'album, la playlist o la coda di origine, anche in riproduzione casuale. Per i podcast torna indietro di qualche secondo, regolabile da 0 a 300 s.
- **Collezioni**: ordinamento casuale in collezioni ed etichette, con «Rimescola»; gli album preferiti di un servizio si ordinano per anno e qualità.
- **In riproduzione**: un pulsante «Condividi» copia il link del brano in corso.
- **Oxygen**: applica o rimuovi un tag su un intero risultato filtrato, dopo anteprima e conferma.
- **Aspetto**: nuovo tema «Mozaiklabs»; la pagina di accesso Mozaiklabs segue la lingua e il tema dell'interfaccia.
- **Lingue**: norvegese bokmål, polacco e greco arrivano in Tune.
- **Android Auto**: Preferiti, Playlist, Radio, Coda e In riproduzione, con i comandi della zona scelta (non ancora provato in auto).

## Miglioramenti

- **In auto**: con Android Auto come con CarPlay, Tune Remote suona su «Questo telefono»; la zona di casa resta intatta.
- **Radio**: le copertine pubblicate dalle stazioni vengono mostrate, con il logo come ripiego; l'elenco si aggiorna senza riavviare Tune; la scelta del flusso rispetta le restrizioni per paese.
- **TIDAL**: la ricerca di una versione Hi-Res prosegue quando la prima risposta è solo in qualità CD, nei limiti del tuo abbonamento.
- **HQPlayer**: i cambi di brano sono più affidabili e un riavvio che non parte viene segnalato.
- **Devialet**: avanzare in un brano Qobuz Hi-Res riprende senza interrompere l'audio; per Devialet AIR, il volume rilevato sull'apparecchio è mostrato a parte.
- **Volume**: volume e muto rispettano le capacità di ogni uscita; l'assistente IA alza il volume solo a piccoli passi.
- **Uscite**: un apparecchio che offre AirPlay e UPnP/DLNA compare in entrambe le forme; ASIO e CoreAudio seguono meglio la griglia di riassegnazione dei canali.
- **Libreria**: elenco compatto degli apparecchi; il filtro Cartelle si applica anche alle aggiunte recenti; la cronologia raggruppa gli ascolti di una stessa stazione.
- **Ascolto remoto**: l'accesso tramite account si configura senza codice QR e gli apparecchi associati all'account possono essere rimossi.

## Correzioni

- **Riproduzione**: un vecchio rilevamento di posizione non termina più il brano dopo uno spostamento; i WAV molto brevi non interrompono più la sequenza; la riproduzione riprende dopo un rifiuto temporaneo di cambio di frequenza.
- **DLNA e zone**: i lettori DLNA ricevono il FLAC multicanale annunciato; cambiare l'apparecchio di una zona in riproduzione ferma quello precedente; un trasferimento rifiutato lascia intatta la riproduzione originale.
- **TIDAL**: corretti gli errori di avvio dovuti ad alcuni link audio.
- **Varie**: estrazione CD dopo Stop, sveglie nell'ordine giusto, nuove playlist visibili subito, il banner di aggiornamento scompare dopo l'installazione, chiavi ListenBrainz e Last.fm con PostgreSQL, backup senza cache HTTP.
- **Sicurezza**: correzione del relay di ascolto remoto; i dettagli saranno pubblicati in un avviso separato.

Spotify è incluso senza una prova di riproduzione reale: facci sapere sul forum se la riproduzione funziona con il tuo account.
