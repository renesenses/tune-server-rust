//! #7371 — le M-SEARCH doit demander des réponses que le pare-feu de Windows
//! laisse passer : il n'accepte les réponses unicast à un multicast que
//! pendant trois secondes quand `tune-server.exe` n'a pas de règle entrante.

use super::{MSEARCH_MX_S, SEARCH_TIMEOUT, parse_ssdp_response, requete_msearch};

/// Fenêtre pendant laquelle le pare-feu de Windows accepte les réponses
/// unicast à un envoi multicast (réglage « Autoriser la réponse en
/// monodiffusion », par défaut).
const FENETRE_PARE_FEU_WINDOWS_S: u64 = 3;

fn mx_de(requete: &str) -> u64 {
    requete
        .lines()
        .find_map(|l| l.strip_prefix("MX:"))
        .expect("le M-SEARCH porte un en-tête MX")
        .trim()
        .parse()
        .expect("MX est un entier")
}

#[test]
fn teac_7371_le_mx_tient_dans_la_fenetre_du_pare_feu_windows() {
    let requete = requete_msearch("urn:schemas-upnp-org:device:MediaRenderer:1");
    let mx = mx_de(&requete);
    assert!(
        (1..FENETRE_PARE_FEU_WINDOWS_S).contains(&mx),
        "MX: {mx} — un renderer peut répondre après {FENETRE_PARE_FEU_WINDOWS_S} s, \
         réponse jetée par le pare-feu de Windows sans règle entrante"
    );
    assert_eq!(mx, MSEARCH_MX_S);
}

#[test]
fn teac_7371_l_ecoute_attend_au_dela_du_mx() {
    assert!(
        SEARCH_TIMEOUT.as_secs() > MSEARCH_MX_S,
        "l'écoute s'arrête avant la fin du délai de réponse demandé"
    );
}

#[test]
fn teac_7371_la_requete_reste_un_m_search_conforme() {
    let requete = requete_msearch("ssdp:all");
    assert!(requete.starts_with("M-SEARCH * HTTP/1.1\r\n"));
    assert!(requete.contains("HOST: 239.255.255.250:1900\r\n"));
    assert!(requete.contains("MAN: \"ssdp:discover\"\r\n"));
    assert!(requete.contains("ST: ssdp:all\r\n"));
    assert!(requete.ends_with("\r\n\r\n"));
    // Une requête n'est pas une réponse : le classeur ne la prend pas pour un appareil.
    assert!(parse_ssdp_response(requete.as_bytes()).is_none());
}
