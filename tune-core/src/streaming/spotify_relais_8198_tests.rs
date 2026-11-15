//! renesenses/tune-server-rust#8198, volet 3 — autorisations entamées et
//! origine de la page Tune, au niveau du service.

use super::*;
use serde_json::json;

const CLIENT_ID: &str = "0123456789abcdef0123456789abcdef";

fn state_du_lien(lien: &str) -> String {
    lien.split_once('?')
        .unwrap()
        .1
        .split('&')
        .find_map(|p| p.strip_prefix("state="))
        .map(decoder_composant)
        .expect("state")
}

/// Une autorisation restée en mémoire plus de `DUREE_TENTATIVE_SECS` ne
/// désigne plus rien, et n'est plus persistée.
#[tokio::test]
async fn une_tentative_perimee_en_memoire_est_refusee() {
    let mut svc = SpotifyService::with_config(Some(CLIENT_ID), None, 8888);
    let ouverte = svc.authenticate(&json!({})).await.expect("ouverture");
    let state = state_du_lien(&ouverte.verification_url.expect("lien"));

    assert!(svc.tentative_pour(Some(&state)).is_ok(), "encore vivante");
    for t in &mut svc.tentatives {
        t.ouverte_le -= DUREE_TENTATIVE_SECS - 5;
    }
    assert!(
        svc.tentative_pour(Some(&state)).is_ok(),
        "vivante juste avant l'échéance"
    );
    for t in &mut svc.tentatives {
        t.ouverte_le -= 10;
    }

    match svc.tentative_pour(Some(&state)) {
        Err(TuneError::Refused { code, .. }) => assert_eq!(code, CODE_STATE_INCONNU),
        autre => panic!("refus attendu, obtenu {autre:?}"),
    }
    match svc
        .authenticate(&json!({"code": "code-essai", "state": state}))
        .await
    {
        Err(TuneError::Refused { code, .. }) => assert_eq!(code, CODE_STATE_INCONNU),
        autre => panic!("refus attendu, obtenu {autre:?}"),
    }
    assert_eq!(svc.tentatives_a_persister(), json!([]));
}

#[test]
fn normaliser_origine_cas_limites() {
    let ok = |o: &str, attendu: &str| {
        assert_eq!(normaliser_origine(o).as_deref(), Some(attendu), "{o}");
    };
    let refuse = |o: &str| assert_eq!(normaliser_origine(o), None, "{o}");

    ok("HTTP://Tune.Local:8888/", "http://tune.local:8888");
    ok(
        "https://192.168.1.20:8443/chemin?x=1#y",
        "https://192.168.1.20:8443",
    );
    ok("  http://10.0.0.5  ", "http://10.0.0.5");
    // IPv6 entre crochets.
    ok("http://[::1]:8888", "http://[::1]:8888");
    ok("http://[FD00::1]", "http://[fd00::1]");
    refuse("http://[::1");
    refuse("http://[zz::1]:8888");
    refuse("http://[::1]x");
    refuse("http://::1:8888");
    refuse("http://a]b");
    // Identifiants.
    refuse("http://moi@192.168.1.20:8888");
    refuse("http://moi:secret@192.168.1.20");
    refuse("http://192.168.1.20@autre.example");
    // Caractères douteux.
    for o in [
        "http://tune local",
        "http://tune\tlocal",
        "http://tune%2elocal",
        "http://tune\\local",
        "http://tune<local>",
        "http://tune\"local",
        "http://tune'local",
        "http://tüne.local",
        "http://tune.local\u{0}",
        "http://tune.local\r\nX: y",
    ] {
        refuse(o);
    }
    // Hôte ou port absents, port invalide.
    refuse("http://");
    refuse("http://:8888");
    refuse("http://tune.local:");
    refuse("http://tune.local:99999");
    refuse("http://tune.local:8x");
    // Schémas.
    refuse("javascript://tune.local");
    refuse("ftp://tune.local");
    refuse("//tune.local");
    refuse("tune.local:8888");
    // 200 caractères au plus.
    let hote_193 = "a".repeat(193);
    let deux_cents = format!("http://{hote_193}");
    assert_eq!(deux_cents.len(), 200);
    ok(&deux_cents, &deux_cents);
    refuse(&format!("http://{hote_193}a"));
}
