Tune 1.0.0-rc14 riprende tutto il contenuto delle pre-release rc13.90 e rc13.91; queste note descrivono cosa è cambiato dalla rc13.91. Grazie ai tester: molti di questi punti vengono dai vostri riscontri.

## Novità

- **TESS, preset Salva e Ricarica**: appena un'impostazione si discosta dal preset scelto, TESS mostra «modificato». **Salva** sovrascrive il preset originale, **Ricarica** vi ritorna. Un preset fornito con Tune, come «Pure», non si sovrascrive: «Salva come…» ne crea una copia.
- **TESS, preset DSD applicato subito**: un preset che cambia il target DSD (per esempio DSD128 → DSD256), caricato in modalità «applica subito», riavvia il brano alla sua posizione, invece di attendere il brano successivo. L'upsampling è una funzione Premium.
- **Headroom**: Automatico, o a mano da −12 a 0 dB a passi di 0,5 dB. Il pannello indica il valore realmente applicato, o spiega perché non lo è.
- **Headroom dell'upsampling verso il DSD**: l'headroom manuale non viene più applicato due volte (−12 dB davano −24 dB; ora sono −15 dB, di cui −3 dB per il modulatore DSD, mostrati su una riga propria).
- **Misura della stanza guidata verso 75 dB(C)**: con un microfono calibrato, lo schermo mostra il livello misurato, lo scarto da 75 dB(C) e un'indicazione sul volume. La regola è prudente, perché la conversione non è ancora stata verificata con un calibratore: Tune non consiglia mai di superare 80 dB(C) stimati e interrompe il segnale a 90 dB SPL. La correzione misurata con il microfono è Premium; quella calcolata senza microfono resta gratuita.
- **Protezione dell'udito**: deselezionare un equalizzatore, passare a PURE o a Bypass all, o togliere una convoluzione, un crossfeed o un downmix mono non fa più salire il livello di colpo: torna al livello precedente con una dissolvenza di circa mezzo secondo. Se nel frattempo avete alzato il volume, si accende la spia «livello ridotto» e «Ripristina livello» rilascia l'eccedenza. Vale anche sulle uscite di rete (UPnP, OAAT).
- **Percorso del segnale fedele**:
  - su un'uscita di rete, segue l'ordine reale del flusso, fino al volume del lettore;
  - lo stadio di un filtro a risposta all'impulso si chiama «Convoluzione»;
  - il filtro mantiene il nome del file .wav o .zip importato;
  - dopo un cambio di target DSD, mostra la frequenza DSD realmente riprodotta, e annuncia la successiva;
  - la finestra diventa verde: l'arancione è riservato agli avvisi veri.
- **Convoluzione**: il pannello mostra la curva del filtro caricato, canale per canale, e la fase su richiesta.
- **Bypass all**: l'icona TESS della barra di riproduzione è attenuata quando TESS è escluso, e la pagina TESS mostra «TESS escluso».
- **Pagina TESS armonizzata**: accento verde TESS, zona in corso accanto al badge, pannello Headroom disattivato su un'uscita di rete, e misura «Senza microfono (stima)» o «Con microfono (consigliato)».
- **Uscita di rete**: la scheda Upsampling dice che l'upsampling non vi si applica.
- **Interfaccia armonizzata**: caratteri integrati, anche offline; Libreria, Live Radio, Server multimediali e Concerti seguono il nuovo stile grafico.

## Correzioni

- **Radio verso un Sonos** (e qualsiasi lettore di rete con un limite di frequenza): una radio a 192 kHz viene servita alla frequenza che il lettore accetta. Prima, il lettore si fermava subito, in continuazione.
- **768 kHz**: l'elenco «Frequenza max» di una zona propone 768 kHz e 1536 kHz.
- **Libreria**: la griglia degli album non si blocca più durante lo scorrimento di una grande collezione.
- **Riproduzione nella scheda del browser**: lo spostamento nel brano sposta davvero la riproduzione, anche da un altro dispositivo.
- **Aiuto**: il banner del supporto prioritario non compare più per un account Premium.
- **Licenza**: la data di scadenza viene letta correttamente, e Impostazioni › Licenza dice perché il livello è gratuito.
- **Convoluzione**: un filtro che amplifica alcune frequenze non satura più.
- **Podcast lunghi**: un episodio il cui host interrompe regolarmente la connessione non viene più interrotto.
- **Pacchetto Debian in un container LXC** (Proxmox): il servizio si avvia anche senza namespace di mount privati.
- **Primo avvio**: la procedura guidata non definisce più «attivati» servizi senza account, e salva la cartella musicale scelta. «Le mie radio» vuoto apre la directory.

## Sicurezza e account

- **Sessione rinnovata**: con «Resta connesso» (non selezionato per impostazione predefinita), la sessione del browser si rinnova finché viene usata, fino a 90 giorni. Altrimenti, 24 h come prima. Il menu dell'avatar propone «Esci da questo server».
- **Recuperare l'accesso amministratore**: eseguite `tune-server --reset-admin` (o `--reinitialiser-admin`) **sulla macchina del server**. Il codice monouso ottenuto, valido 15 minuti, si inserisce in «Password dimenticata?» da quella macchina; senza browser, aggiungete `--password-stdin`. Le sessioni aperte vengono chiuse. La guida di installazione (sezione 9) riporta il comando per ogni sistema.
- **Profilo `admin`**: il profilo amministratore creato d'ufficio si chiama ora `admin`; un nome scelto viene mantenuto, e `default` resta accettato.
- **Password di amministratore fin dalla prima configurazione**: su una nuova installazione, la procedura guidata ve la fa creare.
- **Server senza autenticazione**: l'amministrazione è accettata dalla rete locale, non più solo dalla macchina del server. **Avvertenza**: senza autenticazione, i dispositivi della rete locale possono amministrare il server; attivatela se la vostra rete non è affidabile. L'impostazione «Amministrazione solo da questa macchina» ripristina la regola precedente.

## Telecomandi

- Su un server protetto, le app non ancora aggiornate richiedono la password ogni 24 h, come prima.

## Da verificare a casa vostra

- **«Bypass all» in un preset**: un preset che cambia insieme Bypass all e il target DSD, in modalità «applica subito», riavvia il brano alla sua posizione. Verificato solo al banco di prova: diteci se la ripresa è pulita.
