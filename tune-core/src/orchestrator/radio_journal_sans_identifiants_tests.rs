//! Le journal du chemin radio ne porte aucun identifiant d'une adresse de
//! flux : ni `user:pass@`, ni la valeur de `user=` / `pass=` en requête.
//!
//! Aucun son : un serveur HTTP local répond `200 audio/mpeg` avec quelques
//! octets qui ne sont pas de l'audio ; la sonde échoue après avoir tracé la
//! connexion, et rien n'est jamais joué.
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

#[derive(Clone, Default)]
struct Journal(Arc<Mutex<Vec<u8>>>);
impl Journal {
    fn texte(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}
impl Write for Journal {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Journal {
    type Writer = Journal;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// Un faux serveur de flux : une connexion, un `200 audio/mpeg`, puis il ferme.
fn station_locale() -> u16 {
    let ecoute = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = ecoute.local_addr().unwrap().port();
    std::thread::spawn(move || {
        if let Ok((mut flux, _)) = ecoute.accept() {
            let mut tampon = [0u8; 2048];
            let _ = flux.read(&mut tampon);
            let _ = flux.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: audio/mpeg\r\nConnection: close\r\n\r\npas-de-l-audio",
            );
        }
    });
    port
}

#[test]
fn le_journal_radio_ne_porte_pas_les_identifiants_de_l_adresse() {
    crate::journal_de_test::fiabiliser_la_capture();
    let journal = Journal::default();
    let abonne = tracing_subscriber::fmt()
        .with_writer(journal.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::TRACE)
        .finish();
    let _garde = tracing::subscriber::set_default(abonne);

    let port = station_locale();
    let url = format!(
        "http://auditeur:motdepasse-radio@127.0.0.1:{port}/live/jazz_128?user=auditeur&Pass=secret-radio&format=mp3"
    );
    // La sonde échoue sur des octets qui ne sont pas de l'audio : seul compte
    // ce qu'elle a écrit au journal en se connectant.
    let _ = super::sonder_la_station(&url);

    let texte = journal.texte();
    assert!(
        texte.contains("radio_local_decode_stream_connected"),
        "la connexion au flux doit être tracée :\n{texte}"
    );
    for interdit in [
        "secret-radio",
        "motdepasse-radio",
        "user=auditeur",
        "auditeur:",
    ] {
        assert!(
            !texte.contains(interdit),
            "le journal radio porte « {interdit} » : l'identifiant de l'adresse du flux y est en clair\n{texte}"
        );
    }
    // Le reste de l'adresse reste lisible pour le diagnostic.
    assert!(
        texte.contains(&format!("127.0.0.1:{port}/live/jazz_128?user="))
            && texte.contains("format=mp3"),
        "l'adresse doit rester lisible hors identifiants :\n{texte}"
    );
}

#[test]
fn l_erreur_d_une_station_injoignable_ne_porte_pas_les_identifiants() {
    let ecoute = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = ecoute.local_addr().unwrap().port();
    drop(ecoute);
    let url = format!("http://127.0.0.1:{port}/s.mp3?user=auditeur&pass=secret-radio");
    let erreur = match super::sonder_la_station(&url) {
        Ok(_) => panic!("aucun serveur n'écoute : la sonde doit échouer"),
        Err(e) => e,
    };
    assert!(
        !erreur.contains("secret-radio") && !erreur.contains("user=auditeur"),
        "l'erreur remontée porte l'identifiant de l'adresse : {erreur}"
    );
}
