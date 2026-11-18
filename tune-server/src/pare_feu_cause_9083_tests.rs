//! #9083 — le bandeau « Le pare-feu de Windows filtre Tune » nomme sa cause,
//! et ne s'allume plus quand une autorisation valide couvre le programme lancé.
//!
//! Thierry (fil 2926, rc12, Windows) : réseau Privé, `tune-server.exe` coché
//! Privé dans « Applications autorisées », « figure déjà dans la liste
//! d'exceptions » — et le bandeau reste, avec des consignes déjà suivies.
//! Deux causes restent possibles avec ces captures, et une troisième fait
//! un faux positif :
//!
//! 1. une règle `Block` sur `tune-server.exe` (invite refusée un jour) : elle
//!    l'emporte et n'apparaît pas dans « Applications autorisées » ;
//! 2. l'autorisation porte sur un AUTRE chemin que le programme lancé (Tune
//!    démarré par un raccourci vers un `.cmd`) ;
//! 3. le même fichier écrit autrement (préfixe `\\?\`, nom court 8.3) : la
//!    comparaison de chaînes le prenait pour un autre programme.
//!
//! Les règles sont données sous la forme exacte que rend le script
//! PowerShell du relevé (`SCRIPT_POWERSHELL`), lue par `lire_les_regles`.

use super::*;

const EXE: &str = r"C:\Users\thierry\AppData\Local\Programs\Tune Server\tune-server.exe";

fn sans_variable(_: &str) -> Option<String> {
    None
}

fn sans_canonique(_: &str) -> Option<String> {
    None
}

/// Un réseau Privé seul, carte physique, pare-feu actif.
fn reseau_prive() -> ReleveReseau {
    ReleveReseau {
        reseaux: vec![InterfaceLue {
            categorie: 1,
            ipv4: Some(4),
            route_par_defaut: Some(true),
            physique: Some(true),
        }],
        ..ReleveReseau::default()
    }
}

fn reseau_public() -> ReleveReseau {
    ReleveReseau {
        reseaux: vec![InterfaceLue {
            categorie: 0,
            ipv4: Some(4),
            route_par_defaut: Some(true),
            physique: Some(true),
        }],
        ..ReleveReseau::default()
    }
}

/// Une ligne de la sortie du script : `{"DisplayName", "Program", …}`.
fn ligne(nom: &str, programme: &str, action: i64, profil: i64) -> String {
    format!(
        r#"{{"DisplayName":{nom:?},"Program":{programme:?},"Direction":1,"Action":{action},"Enabled":1,"Profile":{profil}}}"#
    )
}

/// Ce que Windows pose quand on coche « Privé » dans « Autoriser une
/// application » (ou dans l'invite) : deux `Allow` (TCP, UDP) sur Privé, et
/// deux `Block` sur Public pour la case laissée vide.
fn autorisation_privee(programme: &str) -> Vec<String> {
    vec![
        ligne("tune-server.exe", programme, 2, 2),
        ligne("tune-server.exe", programme, 2, 2),
        ligne("tune-server.exe", programme, 4, 4),
        ligne("tune-server.exe", programme, 4, 4),
    ]
}

fn regles(lignes: &[String]) -> Vec<RegleDePareFeu> {
    lire_les_regles(&format!("[{}]", lignes.join(","))).expect("sortie du script")
}

/// Le relevé tel que la route le fait sous Windows : chemins résolus, puis état.
fn etat(
    lignes: &[String],
    releve: &ReleveReseau,
    exe: &str,
    canonique: &dyn Fn(&str) -> Option<String>,
) -> EtatDuPareFeu {
    let (r, exe) = resoudre_les_chemins(&regles(lignes), exe, &sans_variable, canonique);
    etat_du_pare_feu(&r, releve, &exe, &sans_variable)
}

// --- Pas de faux positif ---

/// L'autorisation que montre la capture du testeur, sur le programme lancé :
/// aucun bandeau. Les `Block` du profil Public posés par Windows ne
/// contredisent pas le Privé.
#[test]
fn une_autorisation_privee_sur_le_programme_lance_n_alerte_pas_9083() {
    let e = etat(
        &autorisation_privee(EXE),
        &reseau_prive(),
        EXE,
        &sans_canonique,
    );
    assert!(!e.alerte(), "{e:?}");
    assert_eq!(e.cause, None);
    assert!(e.regles_bloquantes.is_empty());
    assert_eq!(e.profils_couverts, vec![ProfilReseau::Prive]);
}

/// `current_exe()` peut rendre le chemin avec le préfixe verbatim `\\?\` :
/// c'est le même fichier que celui de la règle.
#[test]
fn le_prefixe_verbatim_n_est_pas_un_autre_chemin_9083() {
    let lance = format!(r"\\?\{EXE}");
    let e = etat(
        &autorisation_privee(EXE),
        &reseau_prive(),
        &lance,
        &sans_canonique,
    );
    assert!(!e.alerte(), "{e:?}");
    assert!(meme_chemin(EXE, &lance, &sans_variable));
    assert!(meme_chemin(&format!("\"{EXE}\""), EXE, &sans_variable));
    assert!(meme_chemin(
        r"\\?\UNC\nas\tune\tune-server.exe",
        r"\\NAS\Tune\tune-server.exe",
        &sans_variable
    ));
}

/// Lancé par un `.cmd` sous son nom court 8.3 : le chemin réel (ce que rend
/// `std::fs::canonicalize`) est celui de la règle — pas de bandeau.
#[test]
fn le_nom_court_8_3_resolu_n_alerte_pas_9083() {
    let court = r"C:\Users\THIERR~1\AppData\Local\Programs\TUNESE~1\tune-server.exe";
    let canonique = |c: &str| {
        let n = normaliser_chemin(c);
        (n == normaliser_chemin(court) || n == normaliser_chemin(EXE))
            .then(|| format!(r"\\?\{EXE}"))
    };
    let e = etat(
        &autorisation_privee(EXE),
        &reseau_prive(),
        court,
        &canonique,
    );
    assert!(!e.alerte(), "{e:?}");
    assert_eq!(
        e.programme, EXE,
        "le préfixe verbatim est retiré à l'affichage"
    );
}

// --- La cause nommée ---

/// Cause 1 : une règle `Block` sur le programme lancé, profil Privé. Elle
/// l'emporte sur l'autorisation visible : le bandeau le dit, et la nomme.
#[test]
fn une_regle_de_blocage_est_nommee_9083() {
    let mut lignes = autorisation_privee(EXE);
    lignes.push(ligne("Tune Server (bloqué)", EXE, 4, 2));
    let e = etat(&lignes, &reseau_prive(), EXE, &sans_canonique);
    assert!(e.alerte(), "{e:?}");
    assert!(e.regle_presente, "l'autorisation existe bien");
    assert_eq!(e.cause, Some(CauseNonCouvert::RegleDeBlocage));
    assert_eq!(
        e.regles_bloquantes,
        vec!["Tune Server (bloqué)".to_string()]
    );
}

/// Un `Block` sur le seul profil PUBLIC (case laissée vide) ne bloque pas un
/// réseau Privé ; un `Block` sur une AUTRE copie de `tune-server` non plus.
#[test]
fn un_blocage_qui_ne_vise_pas_le_programme_lance_sur_ce_profil_ne_compte_pas_9083() {
    let mut lignes = autorisation_privee(EXE);
    lignes.push(ligne("vieux", r"D:\Ancien\tune-server.exe", 4, 0));
    let e = etat(&lignes, &reseau_prive(), EXE, &sans_canonique);
    assert!(!e.alerte(), "{e:?}");
    assert!(e.regles_bloquantes.is_empty());
}

/// Cause 2 : l'autorisation porte sur un autre chemin que le programme lancé
/// (`.cmd` qui démarre une autre copie, `tune-server.new.exe` de la mise à
/// jour). Le bandeau montre les deux chemins.
#[test]
fn une_autorisation_sur_un_autre_chemin_montre_les_deux_chemins_9083() {
    let installe = r"C:\Program Files\Tune\tune-server.exe";
    let nouveau = r"C:\Program Files\Tune\tune-server.new.exe";
    let lance = r"D:\Tune\tune-server.exe";
    let mut lignes = autorisation_privee(installe);
    lignes.extend(autorisation_privee(nouveau));
    let e = etat(&lignes, &reseau_prive(), lance, &sans_canonique);
    assert!(e.alerte(), "{e:?}");
    assert!(!e.regle_presente);
    assert_eq!(e.cause, Some(CauseNonCouvert::AutreChemin));
    assert_eq!(e.programme, lance);
    assert_eq!(
        e.chemins_autorises_ailleurs,
        vec![installe.to_string(), nouveau.to_string()]
    );
}

/// Le programme lancé est autorisé, mais pas pour le profil actif (réseau
/// Public, règle Privé) : c'est le cas de #7371, consigne inchangée. Les
/// `Block` Public que Windows pose pour la case laissée vide sont relevés,
/// mais ne sont pas « la » cause : ils ne contredisent aucune autorisation.
#[test]
fn autorise_mais_pas_pour_ce_profil_9083() {
    let e = etat(
        &autorisation_privee(EXE),
        &reseau_public(),
        EXE,
        &sans_canonique,
    );
    assert!(e.alerte());
    assert_eq!(e.cause, Some(CauseNonCouvert::ProfilNonCouvert));
    assert_eq!(e.regles_bloquantes, vec!["tune-server.exe".to_string()]);
}

/// L'invite « Autoriser l'accès » refusée, jamais autorisée ensuite : des
/// `Block` seuls. La case est décochée dans « Autoriser une application »,
/// et « autorisez tune-server.exe » reste la bonne consigne.
#[test]
fn une_invite_refusee_sans_autorisation_reste_aucune_regle_9083() {
    let lignes = [
        ligne("tune-server.exe", EXE, 4, 6),
        ligne("tune-server.exe", EXE, 4, 6),
    ];
    let e = etat(&lignes, &reseau_prive(), EXE, &sans_canonique);
    assert!(e.alerte());
    assert!(!e.regle_presente);
    assert_eq!(e.cause, Some(CauseNonCouvert::AucuneRegle));
    assert_eq!(e.regles_bloquantes, vec!["tune-server.exe".to_string()]);
}

#[test]
fn aucune_regle_9083() {
    let e = etat(&[], &reseau_prive(), EXE, &sans_canonique);
    assert!(e.alerte());
    assert_eq!(e.cause, Some(CauseNonCouvert::AucuneRegle));
}

/// La route rend la cause, les règles et les chemins : l'écran les lit.
#[test]
fn la_route_rend_la_cause_9083() {
    let mut lignes = autorisation_privee(EXE);
    lignes.push(ligne("Bloquer Tune", EXE, 4, 0));
    let e = etat(&lignes, &reseau_prive(), EXE, &sans_canonique);
    let corps = charge_utile(true, Some(&e));
    assert_eq!(corps["alerte"], true);
    assert_eq!(corps["releve"]["cause"], "regle_de_blocage");
    assert_eq!(
        corps["releve"]["regles_bloquantes"],
        serde_json::json!(["Bloquer Tune"])
    );
    assert_eq!(corps["releve"]["programme"], EXE);
    assert_eq!(
        corps["releve"]["chemins_autorises_ailleurs"],
        serde_json::json!([])
    );

    let e = etat(
        &autorisation_privee(EXE),
        &reseau_prive(),
        EXE,
        &sans_canonique,
    );
    let corps = charge_utile(true, Some(&e));
    assert_eq!(corps["alerte"], false);
    assert!(corps["releve"]["cause"].is_null());
}

/// Garde de branchement : sous Windows, la route et le démarrage comparent
/// des chemins RÉSOLUS.
#[test]
fn les_releves_windows_resolvent_les_chemins_9083() {
    let src = include_str!("pare_feu_windows.rs");
    let prod = src.split("#[cfg(test)]").next().unwrap();
    assert_eq!(
        prod.matches("resoudre_les_chemins(regles, exe, &variable, &canonique_windows)")
            .count(),
        1,
        "relever_l_etat doit résoudre les chemins"
    );
    assert_eq!(
        prod.matches("resoudre_les_chemins(&regles, &exe, &variable, &canonique_windows)")
            .count(),
        1,
        "le verdict du démarrage doit résoudre les chemins"
    );
}
