//! Témoins du lot T17 (stockage, chargement, préréglage actif).

use super::*;
use crate::db::backend::DbBackend;
use crate::db::sqlite::SqliteDb;
use crate::db::zone_repo::ZoneRepo;
use crate::tess::chaine::MaillonGreffon;
use crate::tess::greffon::catalogue_de_reference;
use crate::tess::migration::lire_zone;
use serde_json::{Value, json};
use std::sync::Arc;

fn base() -> (SettingsRepo, i64) {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    let backend: Arc<dyn DbBackend> = Arc::new(db);
    let zid = ZoneRepo::with_backend(backend.clone())
        .create("Salon", Some("local"), Some("local:dac"))
        .unwrap();
    (SettingsRepo::with_backend(backend), zid)
}

fn crossfeed() -> ChaineTess {
    ChaineTess {
        plugins: vec![MaillonGreffon {
            id: "crossfeed".into(),
            enabled: true,
            settings: json!({ "amount": 0.3 }),
        }],
        ..ChaineTess::default()
    }
}

fn convolution() -> ChaineTess {
    ChaineTess {
        plugins: vec![MaillonGreffon {
            id: "convolution".into(),
            enabled: true,
            settings: json!({ "ir_path": "/tmp/room.wav" }),
        }],
        ..ChaineTess::default()
    }
}

#[test]
fn sans_cle_la_liste_par_defaut_contient_pure_fourni_par_le_serveur() {
    let (s, _) = base();
    let l = lire_prereglages(&s).unwrap();
    assert_eq!(l.apply_mode, ModeApplication::Immediately);
    let p = l.get(ID_PREREGLAGE_PURE).unwrap();
    assert!(p.builtin);
    assert_eq!(p.name, "Pure — no processing");
    assert!(p.chain.bypass_all && p.chain.plugins.is_empty());
}

#[test]
fn une_cle_illisible_est_refusee_pas_remplacee_par_une_liste_vide() {
    let (s, _) = base();
    s.set(CLE_PREREGLAGES, "{oups").unwrap();
    let e = lire_prereglages(&s).unwrap_err();
    assert_eq!(e.code(), "presets_unreadable");
    // Et rien ne s'écrit par-dessus.
    assert!(enregistrer_la_chaine(&s, "Casque", &crossfeed()).is_err());
    assert_eq!(s.get(CLE_PREREGLAGES).unwrap().as_deref(), Some("{oups"));
}

#[test]
fn enregistrer_puis_remplacer_un_homonyme_et_ne_jamais_ecraser_pure() {
    let (s, _) = base();
    let p = enregistrer_la_chaine(&s, "Evening listening", &crossfeed()).unwrap();
    assert_eq!(p.id, "evening-listening");
    assert!(!p.builtin);
    // Même nom : même id, chaîne remplacée.
    let q = enregistrer_la_chaine(&s, "Evening listening", &convolution()).unwrap();
    assert_eq!(q.id, "evening-listening");
    let l = lire_prereglages(&s).unwrap();
    assert_eq!(l.presets.len(), 2);
    assert_eq!(l.get("evening-listening").unwrap().chain, convolution());
    // Un homonyme de l'id réservé « pure » reçoit un autre id ; Pure reste.
    let r = enregistrer_la_chaine(&s, "Pure", &crossfeed()).unwrap();
    assert_eq!(r.id, "pure-2");
    assert_eq!(
        lire_prereglages(&s).unwrap().get("pure").unwrap().chain,
        PreReglage::pure().chain
    );
    // Nom vide refusé.
    assert_eq!(
        enregistrer_la_chaine(&s, "   ", &crossfeed())
            .unwrap_err()
            .code(),
        "empty_name"
    );
}

#[test]
fn id_depuis_le_nom_suit_la_regle_des_ids() {
    for (nom, id) in [
        ("Evening listening", "evening-listening"),
        ("  Casque — Sennheiser HD650 ", "casque-sennheiser-hd650"),
        ("Écoute du soir", "coute-du-soir"),
        ("!!!", "preset"),
    ] {
        let x = id_depuis_le_nom(nom);
        assert_eq!(x, id);
        assert!(crate::tess::greffon::id_valide(&x), "{x}");
    }
    let long = id_depuis_le_nom(&"a b ".repeat(60));
    assert!(long.len() <= 64 && crate::tess::greffon::id_valide(&long));
}

#[test]
fn charger_ecrit_la_chaine_par_t02_et_rend_le_prereglage_actif() {
    let (s, zid) = base();
    let cat = catalogue_de_reference();
    enregistrer_la_chaine(&s, "Headphones", &crossfeed()).unwrap();
    let chaine = charger(&s, zid, "headphones", &cat, true).unwrap();
    let lue = lire_zone(&s, zid, true).unwrap().chaine;
    assert_eq!(lue, chaine);
    // Écriture croisée T02 : l'ancienne clé du crossfeed suit.
    let ancienne: Value =
        serde_json::from_str(&s.get(&format!("zone_{zid}_crossfeed")).unwrap().unwrap()).unwrap();
    assert_eq!(ancienne["enabled"], true);
    assert_eq!(
        prereglage_actif(&s, zid, &lue).as_deref(),
        Some("headphones")
    );
}

#[test]
fn actif_s_eteint_des_que_la_chaine_de_la_zone_change() {
    let (s, zid) = base();
    let cat = catalogue_de_reference();
    enregistrer_la_chaine(&s, "Headphones", &crossfeed()).unwrap();
    charger(&s, zid, "headphones", &cat, true).unwrap();
    // Une ANCIENNE route (client mobile) coupe le crossfeed.
    s.set(
        &format!("zone_{zid}_crossfeed"),
        r#"{"enabled":false,"amount":0.3}"#,
    )
    .unwrap();
    let lue = lire_zone(&s, zid, true).unwrap().chaine;
    assert_eq!(prereglage_actif(&s, zid, &lue), None);
    // Le préréglage, lui, n'a pas bougé : le recharger le rallume.
    charger(&s, zid, "headphones", &cat, true).unwrap();
    let lue = lire_zone(&s, zid, true).unwrap().chaine;
    assert_eq!(
        prereglage_actif(&s, zid, &lue).as_deref(),
        Some("headphones")
    );
    // Préréglage supprimé : plus d'actif.
    supprimer(&s, "headphones").unwrap();
    assert_eq!(prereglage_actif(&s, zid, &lue), None);
}

#[test]
fn le_droit_se_verifie_au_chargement_pas_a_l_enregistrement() {
    let (s, zid) = base();
    let cat = catalogue_de_reference();
    // Enregistré (compte Premium), puis chargé sans Premium : 402, rien
    // d'écrit, aucun actif.
    enregistrer_la_chaine(&s, "Room", &convolution()).unwrap();
    let avant = lire_zone(&s, zid, false).unwrap().chaine;
    let e = charger(&s, zid, "room", &cat, false).unwrap_err();
    assert_eq!(e.statut_http(), 402);
    assert_eq!(e.code(), "premium_required");
    assert_eq!(e.plugin_id(), Some("convolution"));
    assert_eq!(lire_zone(&s, zid, false).unwrap().chaine, avant);
    assert_eq!(s.get(&cle_prereglage_actif(zid)).unwrap(), None);
    // Il reste enregistré.
    assert!(lire_prereglages(&s).unwrap().get("room").is_some());
    // Préréglage inconnu : 404.
    assert_eq!(
        charger(&s, zid, "nope", &cat, true)
            .unwrap_err()
            .statut_http(),
        404
    );
}

#[test]
fn pure_se_charge_sans_droit_se_supprime_et_le_mode_se_regle() {
    let (s, zid) = base();
    let cat = catalogue_de_reference();
    let c = charger(&s, zid, ID_PREREGLAGE_PURE, &cat, false).unwrap();
    assert!(c.bypass_all);
    let lue = lire_zone(&s, zid, false).unwrap().chaine;
    assert_eq!(prereglage_actif(&s, zid, &lue).as_deref(), Some("pure"));
    let l = definir_le_mode(&s, ModeApplication::AtNextPlayback).unwrap();
    assert_eq!(l.apply_mode, ModeApplication::AtNextPlayback);
    assert_eq!(
        lire_prereglages(&s).unwrap().apply_mode,
        ModeApplication::AtNextPlayback
    );
    supprimer(&s, ID_PREREGLAGE_PURE).unwrap();
    assert!(lire_prereglages(&s).unwrap().get("pure").is_none());
    assert_eq!(supprimer(&s, "pure").unwrap_err().statut_http(), 404);
    // Un mode inconnu (client plus récent) n'interrompt jamais la lecture.
    assert_eq!(
        mode_effectif(ModeApplication::Unknown),
        ModeApplication::AtNextPlayback
    );
}
