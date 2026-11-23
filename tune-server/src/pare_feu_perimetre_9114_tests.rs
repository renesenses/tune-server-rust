//! #9114 (suite de #9083) — le relevé du pare-feu Windows ne lit que les
//! règles dont le programme contient `tune-server`, dans le magasin
//! persistant. Une règle par port, par service, de stratégie de groupe, ou
//! un programme nommé autrement lui échappent : l'alerte peut être fausse.
//! Diagnostic seulement : la route le dit (`perimetre`), le journal aussi.

use super::*;

/// Un réseau Public seul, pare-feu actif, aucune règle : l'alerte s'allume.
fn reseau_public() -> ReleveReseau {
    ReleveReseau {
        reseaux: vec![InterfaceLue {
            categorie: 0,
            ipv4: Some(4),
            route_par_defaut: Some(true),
            physique: Some(true),
        }],
        pare_feu: vec![],
        pare_feu_tiers: 0,
    }
}

#[test]
fn la_route_dit_ce_que_le_releve_ne_lit_pas_9114() {
    let etat = etat_du_pare_feu(&[], &reseau_public(), r"C:\Tune\tune-server.exe", &|_| None);
    let corps = charge_utile(true, Some(&etat), false);
    assert_eq!(corps["alerte"], true, "prémisse : l'alerte s'allume");
    let perimetre = &corps["perimetre"];
    assert_eq!(
        perimetre["lu"],
        serde_json::json!(["regles_par_programme_tune_server"])
    );
    assert_eq!(
        perimetre["non_lu"],
        serde_json::json!([
            "regles_par_port",
            "regles_par_service",
            "strategie_de_groupe",
            "programme_nomme_autrement"
        ]),
        "#9114 : les quatre sources hors de vue sont nommées"
    );
    let message = perimetre["message"].as_str().expect("une phrase");
    assert!(message.contains("par port"), "{message}");
    assert!(message.contains("stratégie de groupe"), "{message}");
    assert!(message.contains("fausse"), "{message}");

    // Un relevé muet sous Windows porte la même limite.
    assert_eq!(charge_utile(true, None, true)["perimetre"], *perimetre);
}

#[test]
fn hors_windows_rien_n_est_releve_ni_annonce_9114() {
    let corps = charge_utile(false, None, false);
    assert!(corps.get("perimetre").is_none(), "{corps}");
}

/// Le script lit toujours les seules règles par programme : si un jour il
/// lit les ports ou le magasin actif, la liste de [`SOURCES_NON_LUES`] doit
/// suivre.
#[test]
fn le_script_ne_lit_que_les_regles_par_programme_9114() {
    assert!(SCRIPT_POWERSHELL.contains("Get-NetFirewallApplicationFilter"));
    assert!(!SCRIPT_POWERSHELL.contains("Get-NetFirewallPortFilter"));
    assert!(!SCRIPT_POWERSHELL.contains("Get-NetFirewallServiceFilter"));
    assert!(!SCRIPT_POWERSHELL.contains("ActiveStore"));
}
