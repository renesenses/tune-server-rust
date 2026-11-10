//! #8008 — une station injoignable dit POURQUOI.
//!
//! Le rapport du 06/11 (rc10) montre `radio_local_decode_failed error=radio
//! HTTP fetch failed: error sending request for url (http://…)`, sans rien
//! d'autre. Refus de connexion, nom introuvable, délai, TLS : on ne peut pas
//! savoir. Le message porte maintenant la chaîne des causes
//! (`decrire_erreur_http`).

/// Un port local qu'aucun serveur n'écoute : la connexion est refusée.
fn port_ferme() -> u16 {
    let ecoute = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = ecoute.local_addr().unwrap().port();
    drop(ecoute);
    port
}

#[test]
fn une_station_injoignable_dit_la_cause_de_l_echec_8008() {
    let url = format!("http://127.0.0.1:{}/station.mp3", port_ferme());
    let erreur = match super::sonder_la_station(&url) {
        Ok(_) => panic!("aucun serveur n'écoute : la sonde doit échouer"),
        Err(e) => e,
    };
    assert!(
        erreur.starts_with("radio HTTP fetch failed: "),
        "le préfixe que lisent les verdicts de relance ne change pas : {erreur}"
    );
    assert!(
        erreur.contains("connection failed") && erreur.to_lowercase().contains("refused"),
        "le message doit nommer la cause (connexion refusée), obtenu : {erreur}"
    );
}
