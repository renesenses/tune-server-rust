//! #7371 — le profil du réseau actif, et ce que la règle de Tune en couvre.
//!
//! TEAC NT-505X (fil VNAV) : le renderer UPnP n'est jamais découvert, seul
//! AirPlay est proposé. La règle « Tune » de l'installeur (rc11) ne couvre
//! que le profil Privé ; le testeur ne peut pas passer son réseau en Privé
//! (message 47). Sur un réseau Public, Tune doit le DIRE : au journal et à
//! l'écran des zones.

use super::*;

const EXE: &str = r"C:\Users\vuanui\AppData\Local\Programs\Tune Server\tune-server.exe";

fn sans_variable(_: &str) -> Option<String> {
    None
}

/// La règle « Tune » que pose l'installeur de la rc11 : entrante, autoriser,
/// profil Privé, sur l'exécutable installé.
fn regle_de_l_installeur() -> RegleDePareFeu {
    RegleDePareFeu {
        display_name: "Tune".into(),
        program: EXE.into(),
        direction: 1,
        action: 2,
        enabled: 1,
        profile: 2,
    }
}

#[test]
fn reseau_public_et_regle_privee_alerte_7371() {
    let etat = etat_du_pare_feu(
        &[regle_de_l_installeur()],
        &[ProfilReseau::Public],
        EXE,
        &sans_variable,
    );
    assert!(etat.regle_presente);
    assert_eq!(etat.profils_couverts, vec![ProfilReseau::Prive]);
    assert_eq!(etat.profils_non_couverts, vec![ProfilReseau::Public]);
    assert!(
        etat.alerte(),
        "un réseau Public hors de la règle doit se dire"
    );
}

#[test]
fn reseau_prive_et_regle_privee_sans_alerte_7371() {
    let etat = etat_du_pare_feu(
        &[regle_de_l_installeur()],
        &[ProfilReseau::Prive],
        EXE,
        &sans_variable,
    );
    assert!(etat.profils_non_couverts.is_empty());
    assert!(!etat.alerte());
}

#[test]
fn sans_regle_le_reseau_prive_alerte_aussi_7371() {
    let etat = etat_du_pare_feu(&[], &[ProfilReseau::Prive], EXE, &sans_variable);
    assert!(!etat.regle_presente);
    assert!(etat.profils_couverts.is_empty());
    assert_eq!(etat.profils_non_couverts, vec![ProfilReseau::Prive]);
    assert!(etat.alerte());
}

/// `Block` l'emporte, profil par profil ; une règle « tous profils » (0)
/// couvre aussi le Public ; une règle sur un autre chemin ne compte pas.
#[test]
fn bloque_tous_profils_et_autre_chemin_7371() {
    let mut bloque_public = regle_de_l_installeur();
    bloque_public.action = 4;
    bloque_public.profile = 4;
    let mut tous = regle_de_l_installeur();
    tous.profile = 0;
    let etat = etat_du_pare_feu(
        &[tous, bloque_public],
        &[ProfilReseau::Public, ProfilReseau::Prive],
        EXE,
        &sans_variable,
    );
    assert_eq!(
        etat.profils_couverts,
        vec![ProfilReseau::Domaine, ProfilReseau::Prive]
    );
    assert_eq!(etat.profils_non_couverts, vec![ProfilReseau::Public]);

    let mut ailleurs = regle_de_l_installeur();
    ailleurs.program = r"D:\Tune\tune-server.exe".into();
    let etat = etat_du_pare_feu(&[ailleurs], &[ProfilReseau::Prive], EXE, &sans_variable);
    assert!(!etat.regle_presente);
    assert!(etat.alerte());
}

#[test]
fn la_sortie_du_script_des_profils_se_lit_7371() {
    assert_eq!(lire_les_profils("").unwrap(), vec![]);
    assert_eq!(lire_les_profils("\u{feff}[]\r\n").unwrap(), vec![]);
    assert_eq!(
        lire_les_profils(r#"{"Categorie":0}"#).unwrap(),
        vec![ProfilReseau::Public]
    );
    // Deux interfaces, dans le désordre, avec un doublon : dédoublonné, ordonné.
    assert_eq!(
        lire_les_profils(r#"[{"Categorie":0},{"Categorie":1},{"Categorie":0},{"Categorie":2}]"#)
            .unwrap(),
        vec![
            ProfilReseau::Domaine,
            ProfilReseau::Prive,
            ProfilReseau::Public
        ]
    );
    assert!(lire_les_profils("Get-NetConnectionProfile : accès refusé").is_err());
}

/// Lecture seule, entiers, et rien qui identifie le réseau du testeur.
#[test]
fn le_script_des_profils_lit_seulement_la_categorie_7371() {
    for interdit in [
        "Set-NetConnectionProfile",
        "netsh",
        "Name",
        "InterfaceAlias",
    ] {
        assert!(!SCRIPT_PROFILS_RESEAU.contains(interdit), "{interdit}");
    }
    assert!(SCRIPT_PROFILS_RESEAU.contains("[int]$_.NetworkCategory"));
}

/// Le corps de la route que lit l'écran des zones.
#[test]
fn la_route_dit_l_alerte_et_les_profils_7371() {
    let etat = etat_du_pare_feu(
        &[regle_de_l_installeur()],
        &[ProfilReseau::Public],
        EXE,
        &sans_variable,
    );
    let corps = charge_utile(true, Some(&etat));
    assert_eq!(corps["windows"], true);
    assert_eq!(corps["alerte"], true);
    assert_eq!(
        corps["releve"]["profils_actifs"],
        serde_json::json!(["public"])
    );
    assert_eq!(
        corps["releve"]["profils_couverts"],
        serde_json::json!(["prive"])
    );
    assert_eq!(
        corps["releve"]["profils_non_couverts"],
        serde_json::json!(["public"])
    );
    assert_eq!(corps["releve"]["regle_presente"], true);

    let hors_windows = charge_utile(false, None);
    assert_eq!(hors_windows["alerte"], false);
    assert!(hors_windows["releve"].is_null());
}

/// Garde de BRANCHEMENT : le relevé retient et journalise l'état, et la
/// route est montée.
#[test]
fn le_releve_et_la_route_sont_branches_7371() {
    let src = include_str!("pare_feu_windows.rs");
    let prod = src.split("#[cfg(test)]").next().unwrap();
    assert!(prod.contains("lancer_powershell(SCRIPT_PROFILS_RESEAU)"));
    assert!(prod.contains("journaliser_l_etat(&etat, &exe);"));
    assert!(prod.contains("retenir_l_etat(etat);"));
    let routes = include_str!("routes/system/mod.rs");
    assert!(routes.contains(r#".route("/pare-feu", get(crate::pare_feu_windows::route_etat))"#));
}
