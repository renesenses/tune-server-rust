Tune 1.0.0-rc13, tredicesima versione candidata di Tune 1.0. Grazie a Gros Bidon, r5yk5, Tades, cpadiou77, Pierre, Daniel LEVY, Lio, sashasha, ajm, Jean Valjean, Semakawa, Jon Rotaetxe, hifi25nl, aqs, Christopher J, FabienM, Begastor e Régis per le loro segnalazioni e prove, ad alec_eiffel per lo Smart Upsampler, a cometdom per Qobuz e a Levente per il design.

## Novità

- **Sovracampionatore**: lo Smart Upsampler di Rémi (alec_eiffel) suona allo stesso livello che senza di esso, e una modifica delle impostazioni si sente subito.
- **Riproduzione e uscite**: Sendspin e Devialet AIR ricevono il suono elaborato (equalizzatore, plugin, sovracampionatore). Lo spegnimento programmato può spegnere il computer; controlli multimediali del desktop su Linux.
- **Crossfeed**: gratuito, con le sue impostazioni e i suoi preset.
- **Convoluzione**: un file di risposta all'impulso per frequenza di campionamento.
- **DSD**: il DSD di un server UPnP viene riprodotto in nativo sui lettori di rete esigenti e due brani DSD si susseguono senza pause.
- **Libreria**: i vostri tag personali diventano tag di Tune; selezione di più album per un'azione di gruppo; filtro per strumento.
- **Sorgente alterata** sostituisce «Hi-Res dubbio», per tutti i formati, con il taglio misurato.
- **Forma d'onda** più fine, anche per i brani CUE, Qobuz e TIDAL (come opzione).
- **Coda**: gli album si comprimono e si spostano con un solo gesto.
- **Home**: 5 playlist «Perché ti piace…» e i widget Artisti più ascoltati, Ascolti frequenti, Ascolta più tardi e Riprendi l'ascolto.
- **Servizi**: pagina «Per te» di TIDAL; 200 novità Qobuz per genere.
- **Tune Circle**: i membri di una cerchia si vedono tra loro se lo accettano, e la vista Mondo mostra tutte le cerchie.
- **Interfaccia web**: turco, autenticazione attivabile dall'interfaccia e cronologia per brano o per ascolto.
- **Tune Client (app desktop)**: trova da solo i server della rete e pilota il server locale.
- **Windows**: avvio di Tune con Windows, proposto dal programma di installazione.
- **App iOS**: CarPlay in 5G più completo, ritorno della barra laterale su iPad.

## Miglioramenti

- **Transizioni**: ReplayGain «Auto» mantiene il guadagno dell'album tra due brani dello stesso album; AAC, Opus e brani CUE si concatenano senza pause.
- **Riproduzione e uscite**: stessa curva di volume in AirPlay 1 e 2; una radio parte più in fretta su un lettore di rete.
- **Windows**: in modalità esclusiva WASAPI, ripiego sull'altra famiglia di frequenze invece di fermare il brano.
- **Devialet AIR**: volume a passi di 1 dB, fino a 192 kHz.
- **DSD**: il rilevamento del DSD nativo è più prudente e viene rivalutato.
- **Grandi librerie**: Cartelle si apre senza attendere, le attività in background non bloccano più il resto.
- **Servizi**: client di accesso Google proprio di Tune per YouTube; Spotify e TIDAL più robusti durante la riproduzione.
- **Interfaccia web**: equalizzatore parametrico più preciso e conferma prima di eliminare.
- **App iOS**: la schermata di blocco pilota la zona anche da ferma; senza DAC, il cursore regola il volume di iOS.
- **Aggiornamento e diagnostica**: l'aggiornamento sostituisce anche i programmi forniti con Tune; le segnalazioni di bug nascondono i nomi.

## Correzioni

- **Riproduzione e uscite**: la ripresa funziona dopo una pausa che aveva rilasciato il DAC; «Ripeti tutto» non confonde più un arresto momentaneo con la fine di un brano; un DAC acceso dopo Tune viene trovato su Windows.
- **Zone**: un dispositivo già collegato nomina la sua zona; con OAAT, una ripresa riapre la sessione invece di restare muta.
- **DLNA, UPnP e OpenHome**: niente più Stop indesiderati su Musicbook Source, muto stabile, Linn Klimax DS/2 riconosciuto.
- **AirPlay**: l'uscita AirPlay 1 non parte più al massimo dopo un riavvio.
- **Percorso del segnale**: non indica più elaborazioni che non avvengono.
- **Libreria**: le edizioni DSD e CD di uno stesso album non vengono più unite; la scrittura del DR non blocca più il server.
- **Servizi**: preferiti Qobuz e TIDAL con la loro durata; le sezioni di Spotify non sono più doppie.
- **Radio**: le emittenti FIP ritrovano il loro logo.
- **Plugin e crossfeed**: un plugin installato agisce senza riavvio; il crossfeed non è più bloccato con OAAT né nel browser.
- **Interfaccia web**: la griglia scorre fino in fondo; il banner «il server non risponde» scompare non appena risponde.

## Sicurezza

- Diversi rafforzamenti della sicurezza.
- I log e le segnalazioni di bug nascondono ora le credenziali contenute in alcuni indirizzi di flusso.
- Sostituisci il percorso (Impostazioni › cartelle musicali) verifica ora le cartelle e non è più accessibile da un altro dispositivo senza autenticazione.

## Cambiamenti di comportamento

- **Tune Circle**: con l'autenticazione, riservato agli amministratori; in sola lettura dall'ascolto remoto.
- **YouTube**: gli account collegati prima di questa versione vanno ricollegati una volta.
- **Radio a fine coda**: di nuovo disattivata per impostazione predefinita; riattivatela nella zona se l'avevate scelta.
- **Equalizzatore**: disattivare l'equalizzatore mantiene il suo preamplificatore fino a «Ripristina il livello».
- **Multiroom**: gruppo gratuito fino a 3 zone; oltre, Premium. Le coppie stereo restano gratuite.
- **CUE con INDEX 02**: inizi dei brani corretti alla prossima scansione, cronologia conservata.
- **Tune OS**: la password temporanea compare sullo schermo della macchina o sulla scheda di avvio, non più nella pagina di accesso; l'accesso remoto resta chiuso finché non viene cambiata.
- **Profili e cronologia**: con l'autenticazione, solo l'amministratore gestisce gli altri profili.
- **Sostituisci il percorso**: senza autenticazione, dalla macchina su cui gira Tune.

## Da verificare a casa vostra

Queste modifiche hanno superato i test automatici senza prove su hardware reale. Diteci cosa constatate:

- **Sendspin e Devialet AIR** con un'elaborazione attiva.
- **Windows**: mini-lettore, modalità esclusiva WASAPI, avvio con Windows.
- **AirPlay 1**: livello all'avvio e tra i brani.
- **DSD verso un lettore di rete** (Atoll ST200, Eversolo).
- **Linn Klimax DS/2** in OpenHome, **Tune Client su macOS** e **CarPlay in 5G**.

## Problemi noti

- **DSD in DoP sull'uscita locale**: dopo un PCM attenuato, può suonare a fondo scala. Tenete basso il volume dell'amplificatore.
- **UPnP dopo un cambio di indirizzo**: un brano potrebbe non essere riprodotto; reindicizzate quel server.
- **Audit di integrità**: alcuni file possono essere segnalati per errore; non eliminate nulla sulla base di questo solo verdetto.
- **Windows ASIO pilotato da un altro server**: niente suono dopo Pausa e poi Riproduci; passate al brano successivo.
- **Dopo l'aggiornamento**: se l'interfaccia non cambia, forzate il ricaricamento (Cmd+Maiusc+R o Ctrl+Maiusc+R).
