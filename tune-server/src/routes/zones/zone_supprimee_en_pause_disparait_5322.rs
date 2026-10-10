//! #5322 — une zone SUPPRIMÉE en pause restait dans `GET /zones`.
//!
//! Terrain (FabienM, 0.9.167, fil 2013, points 9 et 11) : « Je clique sur
//! Supprimer cette zone et je refresh la page, elle est toujours là ». La zone
//! est « Parents » DLNA, EN PAUSE et active ; la fenêtre de confirmation s'est
//! ouverte et a été validée, deux fois. Son diagnostic dit `active_zones: 4`
//! (le compte des zones VISIBLES en base) quand son export en montre cinq :
//! une des cinq est masquée, et pourtant montrée.
//!
//! La cause : #5077 garde dans la liste toute zone masquée qui joue ou est en
//! pause (`ZoneRepo::list_avec_masquees_en_lecture`), pour le DMP-A6 dont la
//! zone avait été masquée SANS que l'utilisateur le demande. La suppression
//! masquait bien la zone — puis l'exception la remettait à l'écran.
//!
//! Et la question qui compte le plus (point 11 : « j'ai perdu ma zone
//! Salon ») : une suppression peut-elle masquer une AUTRE zone que celle
//! visée ? Les bancs ci-dessous le mesurent sur une base SQLite de FICHIER
//! (pas `:memory:`, dont le pool de lecture est un clone de l'écriture), avec
//! les zones du testeur, dont deux « Parents ».

use super::*;
use tune_core::playback::NowPlaying;

struct Banc {
    // Garde le dossier (et la base) en vie le temps du banc.
    _dossier: tempfile::TempDir,
    state: AppState,
    /// (nom, type, appareil) → identifiant
    zones: Vec<(&'static str, &'static str, i64)>,
}

impl Banc {
    fn id(&self, nom: &str, protocole: &str) -> i64 {
        self.zones
            .iter()
            .find(|(n, p, _)| *n == nom && *p == protocole)
            .map(|(_, _, id)| *id)
            .unwrap_or_else(|| panic!("zone {nom} ({protocole}) absente du banc"))
    }
    fn depot(&self) -> ZoneRepo {
        ZoneRepo::with_backend(self.state.backend.clone())
    }
}

/// Les six zones de FabienM, dans une base de fichier neuve.
fn monter() -> Banc {
    let dossier = tempfile::tempdir().unwrap();
    let chemin = dossier.path().join("tune.db");
    let state = AppState::new(chemin.to_str().unwrap(), 0, Default::default()).unwrap();
    let depot = ZoneRepo::with_backend(state.backend.clone());
    let mut zones = Vec::new();
    for (nom, protocole, appareil) in [
        (
            "Bureau",
            "chromecast",
            "chromecast-e3d1886d2514cf61741bee52584c519f",
        ),
        (
            "Enfants",
            "chromecast",
            "chromecast-2fb80b7cd4a33080ee17a2fe4a5809b4",
        ),
        (
            "Parents",
            "chromecast",
            "chromecast-badaed87925be9d77fe8154741a22edb",
        ),
        ("Salle de bain", "dlna", "uuid:RINCON_B8E93782F04801400"),
        ("Salon", "dlna", "uuid:devialet-phantom-salon"),
        (
            "Parents",
            "dlna",
            "uuid:28630ca6-cd4f-4a15-80ae-f170dc890b20",
        ),
    ] {
        let id = depot.create(nom, Some(protocole), Some(appareil)).unwrap();
        zones.push((nom, protocole, id));
    }
    Banc {
        _dossier: dossier,
        state,
        zones,
    }
}

fn piste() -> NowPlaying {
    NowPlaying {
        title: "My Baby Just Cares For Me".into(),
        source: "qobuz".into(),
        duration_ms: 216_000,
        ..Default::default()
    }
}

async fn ids_de_la_liste(state: &AppState) -> Vec<i64> {
    let Json(liste) = list_zones(State(state.clone())).await;
    liste
        .as_array()
        .expect("GET /zones rend un tableau")
        .iter()
        .filter_map(|z| z["id"].as_i64())
        .collect()
}

async fn supprimer(state: &AppState, id: i64) -> StatusCode {
    delete_zone(State(state.clone()), Path(id))
        .await
        .into_response()
        .status()
}

/// 🔴 Le symptôme du point 9 : la zone en pause, supprimée, ne doit plus
/// revenir au rafraîchissement.
#[tokio::test]
async fn une_zone_supprimee_en_pause_disparait_de_la_liste() {
    let banc = monter();
    let parents_dlna = banc.id("Parents", "dlna");
    banc.state.playback.play(parents_dlna, piste()).await;
    banc.state.playback.pause(parents_dlna).await;
    assert!(
        ids_de_la_liste(&banc.state).await.contains(&parents_dlna),
        "prémisse : la zone en pause est listée"
    );

    assert_eq!(
        supprimer(&banc.state, parents_dlna).await,
        StatusCode::NO_CONTENT
    );

    let liste = ids_de_la_liste(&banc.state).await;
    assert!(
        !liste.contains(&parents_dlna),
        "la zone {parents_dlna} a été supprimée (motif suppression_utilisateur) \
         mais `GET /zones` la rend encore parce qu'elle est en pause (#5322) ; \
         liste = {liste:?}"
    );
}

/// Même chose pour une zone qui JOUE, et pour « Tout supprimer ».
#[tokio::test]
async fn une_zone_supprimee_en_lecture_ou_par_tout_supprimer_disparait() {
    let banc = monter();
    let parents_dlna = banc.id("Parents", "dlna");
    let enfants = banc.id("Enfants", "chromecast");
    banc.state.playback.play(parents_dlna, piste()).await;
    banc.state.playback.play(enfants, piste()).await;
    banc.state.playback.pause(enfants).await;

    assert_eq!(
        supprimer(&banc.state, parents_dlna).await,
        StatusCode::NO_CONTENT
    );
    assert!(!ids_de_la_liste(&banc.state).await.contains(&parents_dlna));

    let statut = delete_all_zones(State(banc.state.clone()))
        .await
        .into_response()
        .status();
    assert_eq!(statut, StatusCode::NO_CONTENT);
    let liste = ids_de_la_liste(&banc.state).await;
    assert!(
        liste.is_empty(),
        "après « Tout supprimer », aucune zone ne doit rester, pas même en pause : {liste:?}"
    );
}

/// ⭐ La garde de la question P1 : supprimer « Parents » DLNA (en pause,
/// active, homonyme d'une zone Chromecast) ne masque QU'ELLE — ni l'autre
/// « Parents », ni « Salon », ni aucune autre. Deux fois de suite, comme le
/// testeur l'a fait.
#[tokio::test]
async fn supprimer_une_zone_ne_masque_que_celle_la() {
    let banc = monter();
    let parents_dlna = banc.id("Parents", "dlna");
    banc.state.playback.play(parents_dlna, piste()).await;
    banc.state.playback.pause(parents_dlna).await;

    for passage in 1..=2 {
        assert_eq!(
            supprimer(&banc.state, parents_dlna).await,
            StatusCode::NO_CONTENT,
            "passage {passage}"
        );
        for (nom, protocole, id) in &banc.zones {
            let etat = banc.depot().etat_de_masquage(*id).unwrap().unwrap();
            if *id == parents_dlna {
                assert_eq!(
                    (etat.masquee, etat.motif.as_deref()),
                    (true, Some("suppression_utilisateur")),
                    "passage {passage} : la zone visée doit être masquée"
                );
            } else {
                assert_eq!(
                    (
                        etat.masquee,
                        etat.motif.as_deref(),
                        etat.masquee_le.as_deref()
                    ),
                    (false, None, None),
                    "passage {passage} : supprimer la zone {parents_dlna} a touché \
                     « {nom} » ({protocole}, id {id})"
                );
            }
        }
        let mut attendus: Vec<i64> = banc
            .zones
            .iter()
            .map(|(_, _, id)| *id)
            .filter(|id| *id != parents_dlna)
            .collect();
        let mut liste = ids_de_la_liste(&banc.state).await;
        attendus.sort_unstable();
        liste.sort_unstable();
        assert_eq!(
            liste, attendus,
            "passage {passage} : les cinq autres zones restent listées"
        );
    }
    assert!(
        ids_de_la_liste(&banc.state)
            .await
            .contains(&banc.id("Salon", "dlna")),
        "Salon reste listée"
    );
}

/// Un identifiant qui ne désigne aucune zone : `404`, et rien n'est touché.
/// Avant #5322 la route répondait `204` — le client ne pouvait pas savoir
/// que rien n'avait été masqué.
#[tokio::test]
async fn supprimer_un_identifiant_inconnu_rend_404_et_ne_touche_rien() {
    let banc = monter();
    let inconnu = banc.zones.iter().map(|(_, _, id)| *id).max().unwrap() + 100;
    assert_eq!(supprimer(&banc.state, inconnu).await, StatusCode::NOT_FOUND);
    for (_, _, id) in &banc.zones {
        let etat = banc.depot().etat_de_masquage(*id).unwrap().unwrap();
        assert!(
            !etat.masquee,
            "zone {id} masquée par un DELETE sur {inconnu}"
        );
    }
}

/// Témoin #5077 intact : une zone masquée SANS geste de l'utilisateur
/// (appareil ignoré) et en pause reste montrée.
#[tokio::test]
async fn une_zone_masquee_par_un_ignore_en_pause_reste_montree() {
    let banc = monter();
    let salon = banc.id("Salon", "dlna");
    banc.depot()
        .masquer(
            salon,
            tune_core::db::zone_repo::MotifMasquage::AppareilIgnore,
        )
        .unwrap();
    banc.state.playback.play(salon, piste()).await;
    banc.state.playback.pause(salon).await;
    assert!(
        ids_de_la_liste(&banc.state).await.contains(&salon),
        "l'exception de #5077 doit tenir pour un masquage non demandé"
    );
}

// ---------------------------------------------------------------------------
// #5322, décision de Bertrand : supprimer une zone qui joue ou est en pause
// l'ARRÊTE d'abord (le stop de l'utilisateur, jusqu'à sa sortie), puis la
// masque. Les autres zones, elles, ne reçoivent rien.
// ---------------------------------------------------------------------------

/// Une sortie factice par zone du banc, enregistrée sous l'appareil de la zone.
async fn brancher_des_sorties(banc: &Banc) {
    let depot = banc.depot();
    let mut registre = banc.state.outputs.lock().await;
    for (nom, protocole, id) in &banc.zones {
        let appareil = depot.get(*id).unwrap().unwrap().output_device_id.unwrap();
        registre.register(Box::new(
            tune_core::outputs::mock::MockOutput::new(&appareil, nom).with_type(protocole),
        ));
    }
}

/// Combien de `Stop` la sortie de la zone `id` a REÇUS.
async fn arrets_recus(banc: &Banc, id: i64) -> u64 {
    let appareil = banc
        .depot()
        .get(id)
        .unwrap()
        .unwrap()
        .output_device_id
        .unwrap();
    let registre = banc.state.outputs.lock().await;
    let arc = registre.get(&appareil).expect("sortie enregistrée");
    let sortie = arc.lock().await;
    sortie
        .as_any()
        .downcast_ref::<tune_core::outputs::mock::MockOutput>()
        .expect("MockOutput")
        .stop_call_count()
}

/// 🔴 La zone qui JOUE reçoit le stop sur sa sortie, passe à l'arrêt et
/// disparaît de `GET /zones` ; Salon, qui joue aussi, et les autres zones
/// ne reçoivent aucun stop et ne changent pas.
#[tokio::test]
async fn supprimer_une_zone_qui_joue_l_arrete_puis_la_masque_et_epargne_les_autres() {
    let banc = monter();
    brancher_des_sorties(&banc).await;
    let parents_dlna = banc.id("Parents", "dlna");
    let salon = banc.id("Salon", "dlna");
    banc.state.playback.play(parents_dlna, piste()).await;
    banc.state.playback.play(salon, piste()).await;
    for (_, _, id) in &banc.zones {
        assert_eq!(arrets_recus(&banc, *id).await, 0, "prémisse : aucun stop");
    }

    assert_eq!(
        supprimer(&banc.state, parents_dlna).await,
        StatusCode::NO_CONTENT
    );

    assert!(
        arrets_recus(&banc, parents_dlna).await >= 1,
        "la zone {parents_dlna} jouait : sa sortie devait recevoir un stop avant \
         le masquage (#5322)"
    );
    assert_eq!(
        banc.state.playback.get_state(parents_dlna).await.state,
        PlayState::Stopped,
        "la zone supprimée doit être à l'arrêt"
    );
    let liste = ids_de_la_liste(&banc.state).await;
    assert!(
        !liste.contains(&parents_dlna),
        "zone supprimée encore listée : {liste:?}"
    );
    for (nom, protocole, id) in &banc.zones {
        if *id == parents_dlna {
            continue;
        }
        assert_eq!(
            arrets_recus(&banc, *id).await,
            0,
            "« {nom} » ({protocole}, id {id}) a reçu un stop"
        );
        let etat = banc.depot().etat_de_masquage(*id).unwrap().unwrap();
        assert!(!etat.masquee, "« {nom} » ({protocole}) masquée");
        assert!(
            liste.contains(id),
            "« {nom} » ({protocole}) absente de la liste"
        );
    }
    assert_eq!(
        banc.state.playback.get_state(salon).await.state,
        PlayState::Playing,
        "Salon jouait et doit continuer"
    );
}

/// Une zone EN PAUSE est arrêtée elle aussi ; une zone à l'arrêt ne reçoit
/// pas de stop superflu.
#[tokio::test]
async fn supprimer_une_zone_en_pause_l_arrete_et_une_zone_arretee_ne_recoit_rien() {
    let banc = monter();
    brancher_des_sorties(&banc).await;
    let parents_dlna = banc.id("Parents", "dlna");
    let bureau = banc.id("Bureau", "chromecast");
    banc.state.playback.play(parents_dlna, piste()).await;
    banc.state.playback.pause(parents_dlna).await;

    assert_eq!(
        supprimer(&banc.state, parents_dlna).await,
        StatusCode::NO_CONTENT
    );
    assert!(
        arrets_recus(&banc, parents_dlna).await >= 1,
        "zone en pause non arrêtée"
    );
    assert_eq!(
        banc.state.playback.get_state(parents_dlna).await.state,
        PlayState::Stopped
    );

    assert_eq!(supprimer(&banc.state, bureau).await, StatusCode::NO_CONTENT);
    assert_eq!(
        arrets_recus(&banc, bureau).await,
        0,
        "une zone déjà à l'arrêt n'a pas à recevoir de stop"
    );
}
