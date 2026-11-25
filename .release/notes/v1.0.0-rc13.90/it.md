Tune 1.0.0-rc13.90: **pre-release della rc14**, riservata ai tester. Non è una versione pubblicata: l'aggiornamento automatico non la propone. Grazie a chi la prova.

## Novità

- **TESS, la catena di elaborazione a 64 bit**: tutta l'elaborazione avviene in virgola mobile a 64 bit dall'inizio alla fine, con una sola quantizzazione, alla fine.
- **Lo stesso percorso per tutte le uscite**: uscita locale, UPnP/DLNA, AirPlay, Devialet AIR e OAAT passano da TESS; un'uscita che non può ricevere un'elaborazione lo dice chiaramente.
- **Equalizzatore parametrico** a bande libere (frequenza, guadagno, Q), accanto all'equalizzatore grafico e all'equalizzatore per cuffie (profili AutoEq).
- **Convoluzione**: filtri in .wav, in archivio .zip o in configurazioni Convolver (.cfg), un filtro per frequenza di campionamento, cambio senza interruzioni e latenza annunciata.
- **Correzione ambientale da misura**: un filtro FIR a fase mista calcolato su più posizioni di ascolto.
- **Upsampling verso il DSD** (DSD64, DSD128, DSD256) con un modulatore sigma-delta.
- **Preset di catena**: salvate una catena completa e richiamatela, subito o alla prossima riproduzione.
- **Percorso del segnale**: dalla barra di riproduzione, stadio per stadio, ciò che viene davvero applicato e perché uno stadio non lo è.
- **Plugin nativi** nella catena: attivazione a caldo, formato a 64 bit, ordine e numero di canali rispettati.
- **Interfaccia in 20 lingue**: arrivano olandese, cinese tradizionale, russo e ucraino.
- **Autenticazione**: un pulsante «Disattiva l'autenticazione» in Impostazioni › Sistema › Autenticazione.

## Miglioramenti

- **ISO SACD**: il DSD nativo viene servito ai lettori di rete come DSF virtuale; un DSD trasportato in DoP si annuncia come DSD.
- **Ascolto a distanza**: in 5G, il primo brano parte in FLAC.
- **Dispositivi assenti**: dimenticati automaticamente dopo 30 giorni (regolabile), senza mai eliminare una zona.
- **Cover+**: copertine migliori, anche per le web radio (metadati ICY).
- **Riproduzione di gruppo**: ritardo regolabile per zona; «Precedente» sulla zona principale riavvia anche le zone che la seguono.
- **Libreria e streaming**: le passate in background non bloccano più l'interfaccia, l'ordine alfabetico degli album è mantenuto, artisti simili dalla scheda di un servizio.
- **Messaggi di rifiuto** più brevi, che rimandano all'impostazione giusta.

## Correzioni

- **Le correzioni delle rc13.1 e rc13.2** sono incluse: accesso dell'account amministratore, collegamento dell'account mozaiklabs.fr dalla rete locale, copertine con l'autenticazione attiva, aggiornamento su Windows.
- **UPnP**: Tune non indicizza più se stesso; un brano indicizzato si riproduce dall'indirizzo attuale del suo server.
- **Riproduzione**: precaricamento di 30 s senza buffer scartati; un salto rifiutato dalla sorgente non sfasa più il flusso.
- **Chromecast** si riconnette; la ripubblicazione AirPlay non interrompe più la riproduzione.
- **Podcast**: gli episodi non disponibili sono segnalati correttamente.
- **Segnalazioni del forum**: cronologia, Qobuz, Spotify, pulsante «Indietro», ricerca e iPad.

## Cambiamenti di comportamento

- **Equalizzatore parametrico**: gratuito fino alla rc13, ora fa parte di Tune Premium, come la convoluzione, la correzione ambientale, il declick in tempo reale e i plugin di terze parti. L'equalizzatore grafico, l'equalizzatore per cuffie e il crossfeed restano gratuiti.

## Da verificare a casa vostra

- **Una catena TESS completa** (equalizzatore, crossfeed, convoluzione o correzione ambientale): il percorso del segnale corrisponde a ciò che sentite?
- **L'importazione di un set di convoluzione** e il cambio di filtro durante la riproduzione.
- **L'upsampling verso il DSD** su un DAC compatibile: stabilità, livello, assenza di clic.
- **I preset di catena**, nelle due modalità di applicazione.
- **Un'uscita di rete** (UPnP, AirPlay o OAAT) con un'elaborazione attiva.
- **L'aggiornamento dalla rc13.x** e il pulsante «Disattiva l'autenticazione».

## Problemi noti

- **SACD compressi in DST**: queste immagini ISO non sono ancora lette in modo nativo.
- **VU-meter** su un brano MP3, AAC o Ogg inoltrato: dopo un salto si spengono fino alla fine del brano. Il suono non è toccato.
- **Ascolto a distanza su CarPlay**: la correzione del relay è rinviata alla rc15.
- **Server senza schermo con autenticazione disattivata**: per cambiare l'account mozaiklabs.fr collegato, attivate l'autenticazione o usate un tunnel SSH verso localhost:8888.
