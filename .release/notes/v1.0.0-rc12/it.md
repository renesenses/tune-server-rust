Tune 1.0.0-rc12, dodicesima release candidate di Tune 1.0. Diverse schermate adottano una nuova presentazione disegnata da Levente, designer della comunità Tune.

## Novità

- **Nuova interfaccia**: Tune Circle ridisegnato (Scopri, Messaggi, Playlist condivise, La mia cerchia); scheda dell'album con descrizione, crediti, dettagli e una scheda Versioni con il numero di catalogo; pagina dell'artista con un grande ritratto, cifre e schede; Impostazioni in una colonna più leggibile; Stato del server e cruscotto degli ascolti, con scelta del periodo.
- **Home**: un nuovo widget «Riepilogo degli ascolti (nuova presentazione)», da aggiungere in Home › Modifica, con un pannello Zone; miniature più grandi con anno e genere.
- **Libreria**: fissate i filtri che preferite con il pulsante Filtri; musica classica con Direttore, Ensemble / orchestra e Opera; numero di canali e filtro Stereo; mappa e filtro per paese dell'artista; «Sostituisci il percorso» di una cartella musicale; rilevamento dei falsi Hi-Res, disattivato di default.
- **Scheda tecnica di un brano**: spettrogramma e forma d'onda per canale, anche per i brani Qobuz e TIDAL della libreria.
- **Radio a fine coda**: quando la coda finisce, Tune continua con brani simili, contrassegnati «Radio»; regolabile zona per zona.
- **Spegnimento programmato**: la musica si ferma all'ora scelta; su Tune OS, anche la macchina può spegnersi.
- **Playlist**: numero di ascolti e ultimo ascolto su ogni riga; croce Rimuovi e «Elimina da questa playlist»; rimozione di un brano dalla vostra playlist Qobuz.
- **Radio**: filtri Lossless, Alta qualità e Hi-Res su una sola riga; una stazione dell'elenco si modifica per voi; una radio si aggiunge a una playlist.
- **Riproduzione**: DSD nativo sull'uscita locale Linux (ALSA); frequenza PCM fissa per zona; modalità esclusiva regolata zona per zona; zone eliminate ripristinabili; vera verifica AccurateRip dei CD.
- **DLNA**: flusso continuo tra i brani per i renderer che raccordano male due brani, disattivato di default; il volume master di un gruppo si regola da qualsiasi membro.
- **Streaming**: scelta del paese del catalogo TIDAL; novità Qobuz identiche a quelle del player Qobuz; Spotify con una procedura guidata di connessione, e Spotify Connect suona nella zona scelta.
- **Percorso del segnale**: un clic su un'elaborazione ne apre le impostazioni; la riproduzione raggruppata mostra ogni uscita.
- **Impostazioni e interfaccia**: ricerca per impostazione, dimensione del testo, barra di avanzamento a forma d'onda, un unico campo di ricerca su tutte le schermate, input vocale nell'assistente.

## Miglioramenti

- **Velocità**: raccolte intelligenti e salti alfabetici molto più rapidi su una grande libreria; i grandi elenchi non rallentano più il server.
- **Memoria**: Tune restituisce la memoria liberata durante un lungo ascolto; la memoria della transcodifica è limitata.
- **Avvio del suono**: YouTube parte più in fretta; TIDAL non attende più il download completo del brano su un'uscita locale o OAAT.
- **Statistiche**: un unico totale di brani e album su tutte le schermate, una sola voce per codec, ricerca in tutta la classifica.
- **Tune Circle**: emoji nei Messaggi; quando l'ascolto presso un contatto è chiuso, Tune cerca lo stesso brano nelle vostre sorgenti.
- **App mobili**: CarPlay riproduce direttamente sul telefono, con Raccolte e una scheda Radio; Android ottiene il catalogo TIDAL, la cronologia di radio e podcast e un widget Radio preferite.
- **Windows**: un banner avvisa quando il firewall filtra Tune; badge WASAPI o ASIO sulle zone locali.
- **Docker**: la procedura guidata di condivisione SMB funziona nell'immagine; il lettore CD è utilizzabile nel container.
- **Traduzioni**: plurali corretti nelle 15 lingue.
- **Dipendenze** aggiornate, tra cui SQLite 3.54 integrato.

## Correzioni

- **Interfaccia web**: niente più schermo nero dopo un aggiornamento dell'interfaccia.
- **Silenziamento**: una zona silenziata resta muta quando parte un nuovo brano; una sveglia o un limite di volume non tolgono più il silenziamento.
- **Riproduzione**: un file danneggiato non blocca più la zona, Tune passa al successivo; un episodio di podcast terminato non si riapre più alla fine.
- **AirPlay 2**: dopo una pausa, la riproduzione riprende da dove si era fermata, e parte al volume della zona, mai a piena potenza.
- **DLNA e gruppi**: la riproduzione riprende dopo un errore 701; il brano successivo è preparato in anticipo; un dispositivo che non risponde non blocca più le altre zone; Sonos prende il nome del suo modello.
- **HQPlayer, Devialet e Diretta**: posizione e stato seguiti, riconnessione automatica; l'amplificatore Devialet risponde di nuovo; niente più falso cursore del volume su Diretta.
- **OAAT**: DSD nativo mostrato e bit-perfect; DSD1024 riconosciuto, con ripiego in PCM su un ricevitore limitato.
- **Radio**: niente più stazioni doppie; una radio rifiutata nel vostro paese passa al suo flusso successivo.
- **Spotify, Qobuz e TIDAL**: playlist e preferiti Spotify seguono la nuova API; un rifiuto Qobuz non è più presentato come credenziali errate; il precaricamento TIDAL non ritarda più la riproduzione.
- **Libreria**: gli ascolti sono contati per brano; un trattino non crea più un falso artista; i dischi di un cofanetto sono riconosciuti meglio; un brano senza anno prende quello del suo album.
- **Windows**: un DAC assente non manda più il suono agli altoparlanti del PC; la sorveglianza delle cartelle copre le condivisioni di rete.
- **PostgreSQL 18**: i preferiti UPnP funzionano di nuovo.

## Sicurezza

- Amministrare Tune da un'altra macchina della rete richiede ora di autenticarsi (Tune OS: fino alla nuova immagine).
- I webhook non possono più puntare alla macchina Tune stessa.
- La configurazione di Home Assistant e di Hue richiede i diritti di amministrazione o la macchina Tune stessa; un Home Assistant installato sulla stessa macchina di Tune non è più supportato.

## Problemi noti

- **DSD in DoP sull'uscita locale**: può suonare a fondo scala dopo un PCM attenuato, senza tenere conto del volume. Tenete basso il volume dell'amplificatore quando passate da un brano PCM a un brano DSD in DoP (prossima versione).
- **Audit di integrità**: molti FLAC possono essere segnalati per errore come corrotti; non eliminate alcun file sulla base di questo solo verdetto.
- **Windows ASIO pilotato da un altro server**: niente suono dopo Pausa e poi Riproduci; passate al brano successivo.
- **Dopo l'aggiornamento**: se l'interfaccia non cambia, forzate il ricaricamento (Cmd+Maiusc+R o Ctrl+Maiusc+R). Il primo avvio è più lungo, il tempo di creare due indici.
