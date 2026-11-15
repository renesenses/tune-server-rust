//! #7759 (Didier, fil 2215) — en ASIO, la lecture groupée perdait le gapless
//! et coupait quelques secondes. Décision de Bertrand du 11/11 : la lecture
//! groupée n'est possible qu'en WASAPI. Un groupe dont un membre sort par
//! ASIO est refusé (`400 group_asio_unsupported`, phrase traduite) ; un
//! groupe déjà enregistré dont un membre est passé en ASIO est défait au
//! démarrage.
//!
//! Hors démarrage réel, aucune sortie locale n'est enregistrée : le backend
//! d'une sortie `local:` est alors celui réglé pour la machine
//! (`local_audio_backend`), ce que les essais posent.

use axum::Json;
use axum::extract::{Path, State};
use axum::response::IntoResponse;
use serde_json::{Value, json};
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::db::zone_repo::ZoneRepo;

use std::collections::HashMap;

use super::{
    CreateGroup, GroupRefusal, PatchGroup, create_group, defaire_les_groupes_selon,
    groupes_a_defaire, membre_asio, patch_group, recalculer_les_sorties_doubles,
};
use crate::state::AppState;

async fn serveur(backend: Option<&str>) -> AppState {
    let state = AppState::new(":memory:", 0, crate::config::TuneConfig::default()).unwrap();
    state.license.set_account_premium(true, None).await;
    if let Some(b) = backend {
        SettingsRepo::with_backend(state.backend.clone())
            .set("local_audio_backend", b)
            .unwrap();
    }
    state
}

fn en_francais() -> axum::http::HeaderMap {
    let mut h = axum::http::HeaderMap::new();
    h.insert(axum::http::header::ACCEPT_LANGUAGE, "fr".parse().unwrap());
    h
}

async fn grouper(state: &AppState, meneur: i64, zones: Vec<i64>) -> (u16, Value) {
    let corps: CreateGroup =
        serde_json::from_value(json!({ "leader_id": meneur, "zone_ids": zones })).unwrap();
    let rep = create_group(State(state.clone()), en_francais(), Json(corps))
        .await
        .ok()
        .unwrap()
        .into_response();
    let statut = rep.status().as_u16();
    let octets = axum::body::to_bytes(rep.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        statut,
        serde_json::from_slice(&octets).unwrap_or(Value::Null),
    )
}

fn groupes_enregistres(state: &AppState) -> Vec<Value> {
    SettingsRepo::with_backend(state.backend.clone())
        .get("zone_groups")
        .unwrap()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

#[test]
fn un_seul_membre_asio_suffit_a_refuser() {
    let b = |nom: &str, backend: Option<&str>| (nom.to_string(), backend.map(str::to_string));
    assert_eq!(
        membre_asio(&[b("Cuisine", None), b("TASCAM", Some("ASIO"))]),
        Some(GroupRefusal::AsioUnsupported("TASCAM".into()))
    );
    assert_eq!(
        membre_asio(&[b("SMSL", Some("wasapi")), b("TASCAM", Some("wasapi"))]),
        None
    );
    assert_eq!(
        membre_asio(&[b("Salon", Some("alsa")), b("Cuisine", None)]),
        None
    );
}

#[tokio::test]
async fn un_groupe_avec_une_sortie_asio_est_refuse_en_400() {
    let state = serveur(Some("asio")).await;
    let repo = ZoneRepo::with_backend(state.backend.clone());
    let tascam = repo
        .create("TASCAM", Some("local"), Some("local:US-366 ASIO"))
        .unwrap();
    let cuisine = repo
        .create("Cuisine", Some("dlna"), Some("dlna:uuid-1"))
        .unwrap();

    let (statut, corps) = grouper(&state, cuisine, vec![cuisine, tascam]).await;

    assert_eq!(statut, 400, "{corps}");
    assert_eq!(corps["error"], "group_asio_unsupported");
    assert_eq!(corps["message_key"], "zonegroup.asioUnsupported");
    assert_eq!(corps["params"]["zone"], "TASCAM");
    let phrase = corps["message"].as_str().unwrap();
    assert!(
        phrase.contains("TASCAM") && phrase.contains("WASAPI"),
        "{phrase}"
    );
    assert!(
        groupes_enregistres(&state).is_empty(),
        "rien ne doit être enregistré"
    );
}

#[tokio::test]
async fn deux_sorties_wasapi_se_groupent() {
    let state = serveur(Some("wasapi")).await;
    let repo = ZoneRepo::with_backend(state.backend.clone());
    let smsl = repo
        .create("SMSL", Some("local"), Some("local:SMSL"))
        .unwrap();
    let tascam = repo
        .create("TASCAM", Some("local"), Some("local:TASCAM"))
        .unwrap();

    let (statut, corps) = grouper(&state, smsl, vec![smsl, tascam]).await;

    assert_eq!(statut, 201, "{corps}");
    assert_eq!(groupes_enregistres(&state).len(), 1);
}

/// Sans régression : sur une machine réglée en ASIO, un groupe de sorties
/// réseau n'a aucune sortie ASIO et reste permis.
#[tokio::test]
async fn des_sorties_reseau_se_groupent_meme_machine_en_asio() {
    let state = serveur(Some("asio")).await;
    let repo = ZoneRepo::with_backend(state.backend.clone());
    let a = repo
        .create("Salon", Some("dlna"), Some("dlna:uuid-a"))
        .unwrap();
    let b = repo
        .create("Chambre", Some("airplay"), Some("airplay:b"))
        .unwrap();

    let (statut, corps) = grouper(&state, a, vec![a, b]).await;

    assert_eq!(statut, 201, "{corps}");
}

#[tokio::test]
async fn etendre_un_groupe_a_une_zone_asio_est_refuse() {
    let state = serveur(Some("asio")).await;
    let repo = ZoneRepo::with_backend(state.backend.clone());
    let a = repo
        .create("Salon", Some("dlna"), Some("dlna:uuid-a"))
        .unwrap();
    let b = repo
        .create("Chambre", Some("dlna"), Some("dlna:uuid-b"))
        .unwrap();
    let tascam = repo
        .create("TASCAM", Some("local"), Some("local:US-366 ASIO"))
        .unwrap();
    let (statut, corps) = grouper(&state, a, vec![a, b]).await;
    assert_eq!(statut, 201, "{corps}");
    let gid = groupes_enregistres(&state)[0]["id"].as_i64().unwrap();

    let corps: PatchGroup = serde_json::from_value(json!({ "zone_ids": [a, b, tascam] })).unwrap();
    let rep = patch_group(State(state.clone()), Path(gid), en_francais(), Json(corps))
        .await
        .ok()
        .unwrap()
        .into_response();

    assert_eq!(rep.status().as_u16(), 400);
    assert_eq!(
        groupes_enregistres(&state)[0]["zone_ids"],
        json!([a, b]),
        "le groupe garde sa composition"
    );
}

fn deux_groupes(smsl: i64, tascam: i64, a: i64, b: i64) -> Vec<Value> {
    vec![
        json!({ "id": 1, "group_id": "1", "name": "Bureau", "leader_id": smsl,
                "zone_ids": [smsl, tascam] }),
        json!({ "id": 2, "group_id": "2", "name": "Réseau", "leader_id": a,
                "zone_ids": [a, b] }),
    ]
}

/// Une zone déjà groupée passe en ASIO (réglage de la machine, appliqué au
/// démarrage) : sa sortie enregistrée s'ouvre en ASIO, son groupe est défait
/// avec sa sortie double ; un groupe sans sortie ASIO reste.
#[tokio::test]
async fn un_groupe_dont_une_sortie_enregistree_est_asio_est_defait() {
    let state = serveur(None).await;
    let repo = ZoneRepo::with_backend(state.backend.clone());
    let smsl = repo
        .create("SMSL", Some("local"), Some("local:SMSL"))
        .unwrap();
    let tascam = repo
        .create("TASCAM", Some("local"), Some("local:TASCAM"))
        .unwrap();
    let a = repo
        .create("Salon", Some("dlna"), Some("dlna:uuid-a"))
        .unwrap();
    let b = repo
        .create("Chambre", Some("dlna"), Some("dlna:uuid-b"))
        .unwrap();
    let zones = repo.list().unwrap();
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let cle = tune_core::outputs::sortie_double::cle_de_zone(smsl);
    settings
        .set(
            &cle,
            &tune_core::outputs::sortie_double::ecrire_la_valeur(&["local:TASCAM".to_string()]),
        )
        .unwrap();
    let mut groupes = deux_groupes(smsl, tascam, a, b);
    let backends: HashMap<i64, String> =
        [(smsl, "wasapi".to_string()), (tascam, "asio".to_string())].into();

    let defaits = defaire_les_groupes_selon(&state, &settings, &mut groupes, &zones, &backends);

    assert_eq!(defaits, vec![1]);
    assert_eq!(groupes.len(), 1, "{groupes:?}");
    assert_eq!(groupes[0]["id"], 2);
    assert_eq!(groupes_enregistres(&state).len(), 1);
    assert_eq!(
        settings.get(&cle).unwrap(),
        None,
        "la sortie double part avec le groupe"
    );
}

/// Sans régression : les mêmes groupes, sorties toutes en WASAPI, restent.
#[test]
fn des_sorties_wasapi_enregistrees_gardent_leur_groupe() {
    let zones = vec![];
    let backends: HashMap<i64, String> =
        [(1, "wasapi".to_string()), (2, "wasapi".to_string())].into();
    assert!(groupes_a_defaire(&deux_groupes(1, 2, 3, 4), &zones, &backends).is_empty());
}

/// Défaire est irréversible : une sortie locale ABSENTE du registre (DAC
/// débranché, sortie WASAPI vue plus tard) ne défait rien, même sur une
/// machine réglée en ASIO. Le démarrage ne retombe pas sur le réglage.
#[tokio::test]
async fn au_demarrage_une_sortie_absente_ne_defait_pas_le_groupe() {
    let state = serveur(Some("asio")).await;
    let repo = ZoneRepo::with_backend(state.backend.clone());
    let smsl = repo
        .create("SMSL", Some("local"), Some("local:SMSL"))
        .unwrap();
    let tascam = repo
        .create("TASCAM", Some("local"), Some("local:TASCAM"))
        .unwrap();
    let a = repo
        .create("Salon", Some("dlna"), Some("dlna:uuid-a"))
        .unwrap();
    let b = repo
        .create("Chambre", Some("dlna"), Some("dlna:uuid-b"))
        .unwrap();
    SettingsRepo::with_backend(state.backend.clone())
        .set(
            "zone_groups",
            &serde_json::to_string(&deux_groupes(smsl, tascam, a, b)).unwrap(),
        )
        .unwrap();

    recalculer_les_sorties_doubles(&state).await;

    assert_eq!(groupes_enregistres(&state).len(), 2);
}

#[test]
fn la_phrase_existe_dans_les_quinze_langues() {
    for lang in [
        "fr", "en", "de", "es", "it", "zh", "ja", "ko", "ro", "sv", "hu", "vi", "nb", "pl", "el",
    ] {
        let t = crate::i18n::t(lang, "zonegroup.asioUnsupported");
        assert!(
            t.contains("{zone}") && t.contains("ASIO") && t.contains("WASAPI"),
            "{lang} : {t}"
        );
        if lang != "en" {
            assert_ne!(
                t,
                crate::i18n::t("en", "zonegroup.asioUnsupported"),
                "{lang} retombe sur l'anglais : traduction manquante"
            );
        }
    }
}
