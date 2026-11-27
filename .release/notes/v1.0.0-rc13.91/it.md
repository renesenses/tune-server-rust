Tune 1.0.0-rc13.91: **seconda pre-release della rc14**, riservata ai tester. Non è una versione pubblicata: l'aggiornamento automatico non la propone. Riprende tutto il contenuto della rc13.90. Grazie a chi la prova.

## Novità

- **Equalizzatori combinabili in serie**: equalizzatore grafico, parametrico, per cuffie e correzione ambientale si susseguono nella stessa catena TESS; applicare una correzione ambientale non sovrascrive più gli altri equalizzatori. I guadagni dell'equalizzatore parametrico sono ampliati.
- **Bypass all completo**: un solo gesto esclude tutta la catena TESS e la ripristina identica.
- **Plugin Cuffie** basato sul database OPRA: il profilo delle vostre cuffie viene riconosciuto, con preamplificazione e livello.
- **Plugin Speaker Setup** (regolazione dei diffusori): distanza, guadagno e fase, canale per canale.
- **Upsampling verso il DSD sull'uscita locale**: un modulatore sigma-delta dedicato, e le capacità DSD dell'uscita vengono pubblicate.
- **Destinazione dell'upsampling a scelta**: in TESS, una schermata permette di scegliere verso cosa fare l'upsampling, in PCM o in DSD.
- **Test di livello del microfono** prima della misura dell'ambiente.
- **Misura alla frequenza nativa del microfono**, da 44,1 a 192 kHz.
- **Impostazioni TESS al volo**: disattivare uno stadio ha effetto subito, e la preamplificazione resta limitata.

## Miglioramenti

- **Correzione ambientale**: la misura di verifica sente la correzione della zona; gli scarti non corretti sono spiegati, e il limite superiore è regolabile fino a 1 kHz.
- **Correzione ambientale**: i dati inseriti, l'ultima misura e la correzione applicata si ritrovano tornando sulla schermata; indicazioni sul posizionamento del microfono e un riquadro «Prima della misura» guidano la misura.
- **Misura in modalità PURE**: il volume è forzato al 100 % per la durata dello sweep, poi riportato com'era.
- **Percorso del segnale**: uno stadio spento (upsampling, convoluzione) dice perché, in TESS e nella pagina del plugin; su Windows in modalità condivisa, l'upsampling punta alla frequenza del mixer.
- **Crediti**: lo strumento invece di «Musicista», l'autore distinto dal compositore, un nome per ruolo.
- **Scansione**: il codice a barre presente nei tag completa l'album se non ne ha uno.
- **Stato del server**: compare un avviso se la cache delle copertine non può essere creata.

## Correzioni

- **Calibrazione**: lo sweep non si ferma più prima della durata annunciata e non resta più bloccato in coda; una zona senza uscita viene rifiutata chiaramente.
- **Calibrazione**: un picco al limite massimo non blocca più le bande successive.
- **RT60**: «non misurabile (rumore)» invece di un valore errato; i messaggi di errore restano leggibili almeno 10 s.
- **Avviso di frequenza**: ora compare solo al di fuori delle frequenze accettate.
- **Radio**: il cambio di stazione non viene più sovrascritto dalla stazione precedente.
- **Copertine**: su una nuova installazione, la cache viene creata nella cartella dei dati.
- **TESS**: un solo avviso «brano successivo», e l'aiuto del pulsante «Applica» è corretto.

## Cambiamenti di comportamento

- **Modifica della catena TESS durante la riproduzione**: uno stadio disattivato si interrompe subito; uno stadio aggiunto o riattivato si sente dalla riproduzione successiva.

## Da verificare a casa vostra

Questi punti non sono ancora stati testati su hardware reale:

- **L'upsampling verso il DSD su un vero DAC DSD**: stabilità, livello, assenza di clic.
- **La misura dell'ambiente a 44,1 kHz e a 96 kHz** su una vera interfaccia audio, con il test di livello del microfono.
- **Su Windows, in WASAPI condiviso**: riproduzione e upsampling.
- **Speaker Setup su un'uscita di rete** (UPnP, AirPlay o OAAT): distanza, guadagno e fase.

E inoltre:

- **Più equalizzatori in serie** con Bypass all: il percorso del segnale corrisponde a ciò che sentite?
- **Il plugin Cuffie** con le vostre cuffie.
- **L'aggiornamento dalla rc13.90.**

## Problemi noti

- **SACD compressi in DST**: queste immagini ISO non sono ancora lette in modo nativo.
- **VU-meter** su un brano MP3, AAC o Ogg inoltrato: dopo un salto si spengono fino alla fine del brano. Il suono non è toccato.
- **Ascolto a distanza su CarPlay**: la correzione del relay è rinviata alla rc15.
- **Server senza schermo con autenticazione disattivata**: per cambiare l'account mozaiklabs.fr collegato, attivate l'autenticazione o usate un tunnel SSH verso localhost:8888.
