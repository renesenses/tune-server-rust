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

/// Une carte physique qui porte le réseau local (route par défaut, Internet).
fn lan(categorie: i64) -> InterfaceLue {
    InterfaceLue {
        categorie,
        ipv4: Some(4),
        route_par_defaut: Some(true),
        physique: Some(true),
    }
}

/// Un relevé où seuls ces profils sont connectés, pare-feu actif partout.
fn reseaux(profils: &[ProfilReseau]) -> ReleveReseau {
    let categorie = |p: &ProfilReseau| match p {
        ProfilReseau::Public => 0,
        ProfilReseau::Prive => 1,
        ProfilReseau::Domaine => 2,
    };
    ReleveReseau {
        reseaux: profils.iter().map(|p| lan(categorie(p))).collect(),
        ..ReleveReseau::default()
    }
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
        &reseaux(&[ProfilReseau::Public]),
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
        &reseaux(&[ProfilReseau::Prive]),
        EXE,
        &sans_variable,
    );
    assert!(etat.profils_non_couverts.is_empty());
    assert!(!etat.alerte());
}

#[test]
fn sans_regle_le_reseau_prive_alerte_aussi_7371() {
    let etat = etat_du_pare_feu(&[], &reseaux(&[ProfilReseau::Prive]), EXE, &sans_variable);
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
        &reseaux(&[ProfilReseau::Public]),
        EXE,
        &sans_variable,
    );
    assert_eq!(
        etat.profils_couverts,
        vec![ProfilReseau::Domaine, ProfilReseau::Prive]
    );
    assert_eq!(etat.profils_non_couverts, vec![ProfilReseau::Public]);
    assert!(etat.alerte());

    let mut ailleurs = regle_de_l_installeur();
    ailleurs.program = r"D:\Tune\tune-server.exe".into();
    let etat = etat_du_pare_feu(
        &[ailleurs],
        &reseaux(&[ProfilReseau::Prive]),
        EXE,
        &sans_variable,
    );
    assert!(!etat.regle_presente);
    assert!(etat.alerte());
}

/// La sortie réelle du script sur un PC de bureau avec Hyper-V, WSL et un VPN
/// coupé : le LAN Wi-Fi est Privé, les vEthernet et le tunnel sont Public.
const SORTIE_PC_AVEC_HYPER_V: &str = r#"{"Reseaux":[
  {"Categorie":1,"Ipv4":4,"RouteParDefaut":true,"Physique":true},
  {"Categorie":0,"Ipv4":1,"RouteParDefaut":false,"Physique":false},
  {"Categorie":0,"Ipv4":3,"RouteParDefaut":false,"Physique":false},
  {"Categorie":0,"Ipv4":1,"RouteParDefaut":false,"Physique":false}],
 "PareFeu":[{"Nom":"Domain","Actif":1},{"Nom":"Private","Actif":1},{"Nom":"Public","Actif":1}],
 "PareFeuTiers":0}"#;

/// Relecture du 12/11 : les adaptateurs virtuels (vEthernet Hyper-V/WSL/
/// Docker, VPN) sont presque toujours Public ; ils ne doivent pas faire
/// alerter un PC dont le vrai réseau est Privé.
#[test]
fn interface_virtuelle_publique_et_lan_prive_sans_alerte_7371() {
    let releve = lire_le_releve_reseau(SORTIE_PC_AVEC_HYPER_V).unwrap();
    assert_eq!(releve.reseaux.len(), 4);
    let etat = etat_du_pare_feu(&[regle_de_l_installeur()], &releve, EXE, &sans_variable);
    assert_eq!(etat.profils_actifs, vec![ProfilReseau::Prive]);
    assert_eq!(etat.profils_autres_reseaux, vec![ProfilReseau::Public]);
    assert!(etat.profils_non_couverts.is_empty());
    assert!(!etat.alerte(), "une vEthernet Public ne doit pas alerter");
    // Le journal le mentionne pour mémoire, sans conclure.
    assert_eq!(etat.un_reseau_non_couvert(), vec![ProfilReseau::Public]);
}

/// Le même PC sur un réseau Public pour de vrai : l'alerte reste.
#[test]
fn lan_public_avec_interface_virtuelle_alerte_toujours_7371() {
    let sortie = SORTIE_PC_AVEC_HYPER_V.replacen(r#""Categorie":1"#, r#""Categorie":0"#, 1);
    let releve = lire_le_releve_reseau(&sortie).unwrap();
    let etat = etat_du_pare_feu(&[regle_de_l_installeur()], &releve, EXE, &sans_variable);
    assert_eq!(etat.profils_actifs, vec![ProfilReseau::Public]);
    assert!(etat.alerte());
}

/// Carte physique liée à un commutateur Hyper-V externe : le LAN passe par
/// une vEthernet, la seule qui porte la route par défaut.
#[test]
fn sans_carte_physique_la_route_par_defaut_decide_7371() {
    let releve = lire_le_releve_reseau(
        r#"{"Reseaux":[
          {"Categorie":1,"Ipv4":4,"RouteParDefaut":true,"Physique":false},
          {"Categorie":0,"Ipv4":3,"RouteParDefaut":false,"Physique":false}],
         "PareFeu":[],"PareFeuTiers":0}"#,
    )
    .unwrap();
    let etat = etat_du_pare_feu(&[regle_de_l_installeur()], &releve, EXE, &sans_variable);
    assert_eq!(etat.profils_actifs, vec![ProfilReseau::Prive]);
    assert!(!etat.alerte());
}

/// Deux réseaux locaux, l'un Privé, l'autre Public : on ne sait pas où est
/// le renderer, on ne conclut pas que le DLNA est filtré.
#[test]
fn un_des_reseaux_locaux_public_ne_conclut_pas_7371() {
    let etat = etat_du_pare_feu(
        &[regle_de_l_installeur()],
        &reseaux(&[ProfilReseau::Prive, ProfilReseau::Public]),
        EXE,
        &sans_variable,
    );
    assert_eq!(etat.profils_non_couverts, vec![ProfilReseau::Public]);
    assert!(!etat.alerte());
    assert_eq!(etat.un_reseau_non_couvert(), vec![ProfilReseau::Public]);
}

/// Pare-feu Windows coupé sur le profil du réseau : rien n'est filtré.
#[test]
fn pare_feu_coupe_sans_alerte_7371() {
    let releve = lire_le_releve_reseau(
        r#"{"Reseaux":{"Categorie":0,"Ipv4":4,"RouteParDefaut":true,"Physique":true},
            "PareFeu":[{"Nom":"Domain","Actif":1},{"Nom":"Private","Actif":1},{"Nom":"Public","Actif":0}],
            "PareFeuTiers":0}"#,
    )
    .unwrap();
    let etat = etat_du_pare_feu(&[regle_de_l_installeur()], &releve, EXE, &sans_variable);
    assert_eq!(etat.profils_actifs, vec![ProfilReseau::Public]);
    assert_eq!(etat.profils_pare_feu_coupe, vec![ProfilReseau::Public]);
    assert!(etat.profils_non_couverts.is_empty());
    assert!(
        !etat.alerte(),
        "pare-feu coupé : la découverte n'est pas filtrée"
    );

    // « Non configuré » (2) vaut actif, comme le défaut de Windows.
    let releve = lire_le_releve_reseau(
        r#"{"Reseaux":[{"Categorie":0,"Ipv4":4,"RouteParDefaut":true,"Physique":true}],
            "PareFeu":[{"Nom":"Public","Actif":2}],"PareFeuTiers":0}"#,
    )
    .unwrap();
    assert!(etat_du_pare_feu(&[regle_de_l_installeur()], &releve, EXE, &sans_variable).alerte());
}

/// Un pare-feu tiers est inscrit : on ne sait pas ce qu'il filtre.
#[test]
fn pare_feu_tiers_sans_alerte_7371() {
    let releve = lire_le_releve_reseau(
        r#"{"Reseaux":[{"Categorie":0,"Ipv4":4,"RouteParDefaut":true,"Physique":true}],
            "PareFeu":[{"Nom":"Public","Actif":1}],"PareFeuTiers":1}"#,
    )
    .unwrap();
    let etat = etat_du_pare_feu(&[regle_de_l_installeur()], &releve, EXE, &sans_variable);
    assert!(etat.pare_feu_tiers);
    assert!(!etat.alerte());
}

#[test]
fn la_sortie_du_script_des_profils_se_lit_7371() {
    assert_eq!(lire_le_releve_reseau("").unwrap(), ReleveReseau::default());
    assert_eq!(
        lire_le_releve_reseau("\u{feff}[]\r\n").unwrap(),
        ReleveReseau::default()
    );
    // Listes vides ou nulles, objet seul au lieu d'un tableau.
    let r = lire_le_releve_reseau(
        r#"{"Reseaux":null,"PareFeu":{"Nom":"Private","Actif":0},"PareFeuTiers":0}"#,
    )
    .unwrap();
    assert!(r.reseaux.is_empty());
    assert_eq!(r.pare_feu.len(), 1);
    // Champs inconnus absents : l'interface compte comme réseau local.
    let r = lire_le_releve_reseau(r#"[{"Categorie":0},{"Categorie":1}]"#).unwrap();
    let etat = etat_du_pare_feu(&[regle_de_l_installeur()], &r, EXE, &sans_variable);
    assert_eq!(
        etat.profils_actifs,
        vec![ProfilReseau::Prive, ProfilReseau::Public]
    );
    assert!(lire_le_releve_reseau("Get-NetConnectionProfile : accès refusé").is_err());
    assert!(lire_le_releve_reseau("42").is_err());
}

/// Lecture seule : aucune cmdlet qui écrit (le comportement de PowerShell
/// ne se teste pas hors Windows ; seule cette liste noire le peut).
#[test]
fn les_scripts_sont_en_lecture_seule_7371() {
    for script in [SCRIPT_PROFILS_RESEAU, SCRIPT_POWERSHELL] {
        for interdit in [
            "Set-", "New-", "Remove-", "Enable-", "Disable-", "netsh", "Invoke-",
        ] {
            assert!(!script.contains(interdit), "{interdit}");
        }
    }
}

/// Le corps de la route que lit l'écran des zones.
#[test]
fn la_route_dit_l_alerte_et_les_profils_7371() {
    let etat = etat_du_pare_feu(
        &[regle_de_l_installeur()],
        &reseaux(&[ProfilReseau::Public]),
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

/// Hors Windows, la route répond sans rien relever.
#[tokio::test]
async fn la_route_hors_windows_ne_releve_rien_7371() {
    if cfg!(target_os = "windows") {
        return;
    }
    let axum::Json(corps) = route_etat().await;
    assert_eq!(corps, charge_utile(false, None));
}

/// Le cache de la route : servi tant qu'il a moins de 30 s, relu ensuite
/// (le passage du réseau en Privé se voit sans redémarrage) ; un relevé muet
/// garde l'état précédent.
#[tokio::test]
async fn le_cache_est_relu_apres_expiration_7371() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    let cache = tokio::sync::Mutex::new(CacheDuReleve::default());
    let releves = AtomicUsize::new(0);
    let public = etat_du_pare_feu(
        &[regle_de_l_installeur()],
        &reseaux(&[ProfilReseau::Public]),
        EXE,
        &sans_variable,
    );
    let prive = etat_du_pare_feu(
        &[regle_de_l_installeur()],
        &reseaux(&[ProfilReseau::Prive]),
        EXE,
        &sans_variable,
    );
    let t0 = Instant::now();
    let relever = |etat: Option<EtatDuPareFeu>| {
        let releves = &releves;
        move |_precedent: Option<EtatDuPareFeu>| async move {
            releves.fetch_add(1, Ordering::SeqCst);
            etat
        }
    };

    // Premier appel : relevé, réseau Public.
    let e = etat_avec_cache(&cache, t0, DUREE_DU_CACHE, relever(Some(public.clone()))).await;
    assert_eq!(e, Some(public.clone()));
    assert_eq!(releves.load(Ordering::SeqCst), 1);

    // 10 s plus tard, le réseau est passé en Privé, mais le cache sert encore.
    let e = etat_avec_cache(
        &cache,
        t0 + Duration::from_secs(10),
        DUREE_DU_CACHE,
        relever(Some(prive.clone())),
    )
    .await;
    assert_eq!(e, Some(public.clone()));
    assert_eq!(releves.load(Ordering::SeqCst), 1);

    // Après expiration : relu, l'alerte tombe.
    let t1 = t0 + DUREE_DU_CACHE + Duration::from_secs(1);
    let e = etat_avec_cache(&cache, t1, DUREE_DU_CACHE, relever(Some(prive.clone()))).await;
    assert_eq!(e, Some(prive.clone()));
    assert!(!e.unwrap().alerte());
    assert_eq!(releves.load(Ordering::SeqCst), 2);

    // Relevé muet après une nouvelle expiration : l'état précédent reste.
    let t2 = t1 + DUREE_DU_CACHE + Duration::from_secs(1);
    let e = etat_avec_cache(&cache, t2, DUREE_DU_CACHE, relever(None)).await;
    assert_eq!(e, Some(prive));
    assert_eq!(releves.load(Ordering::SeqCst), 3);
}
