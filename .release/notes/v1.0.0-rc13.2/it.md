Tune 1.0.0-rc13.2, versione correttiva della rc13.1.

## Correzioni

- Accesso: la password dell'account amministratore viene verificata allo stesso modo sulla macchina del server e sulla rete locale; messaggio di errore più chiaro (nome utente: default).
- Se non riesci più ad accedere, usa il nome utente default e l'ultima password inserita sulla macchina del server.
- L'account mozaiklabs.fr ora può essere collegato da qualsiasi browser della rete locale (iPad, telefono) quando non è ancora collegato alcun account.
- Dalla rete locale si può anche ricollegare lo stesso account. Sostituirlo con un altro account, o scollegarlo, resta riservato al computer del server o a un amministratore. Il nome e l'indirizzo e-mail dell'account collegato sono visibili dalla rete locale.
- Windows: l'aggiornamento da Tune funziona di nuovo. Dalla rc13 e dalla rc13.1 bisognava usare il programma di installazione.
- Server Tune OS rimasti alla rc12 che non si aggiornano più: il nuovo strumento «Débloquer Tune» (Windows, Mac) risolve il problema in pochi clic, senza alcun comando. Se il database è stato copiato da un altro computer e non ha l'account default, chiede nome utente e password del suo account amministratore. Download: https://github.com/renesenses/tune-server-rust/releases/tag/debloquer-tune-v1
