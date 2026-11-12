//! #8445 (Jon, fil 2760) — les commandes d'un groupe depuis n'importe quel
//! membre : pause, reprise et arrêt donnés sur une suiveuse valent pour tout
//! le groupe, et le volume maître arrive à chaque membre, décalages compris,
//! sauf à ceux en volume fixe.
//!
//! Banc : trois zones DLNA factices (`MockOutput`) groupées — A meneuse, B et
//! C suiveuses —, une quatrième D hors groupe, le relais lancé comme au
//! démarrage, les routes publiques. Aucun son.
//!
//! Contre-épreuve : retirer l'appel à `relayer_le_geste_d_un_membre` de la
//! route `pause` fait tomber `une_pause_sur_une_suiveuse_met_tout_le_groupe_en_pause` (A
//! et C restent en lecture) ; ne plus relire les décalages enregistrés dans
//! `appliquer_le_volume_du_groupe` (revenir aux seuls `body.offsets`) fait
//! tomber `le_volume_maitre_arrive_a_chaque_membre_sauf_au_volume_fixe` (B
//! reçoit 0,40 au lieu de 0,30).

use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::db::zone_repo::ZoneRepo;
use tune_core::outputs::mock::MockOutput;
use tune_core::playback::{NowPlaying, PlayState};

use super::*;

// --- Règles pures -----------------------------------------------------------

#[test]
fn seules_les_suiveuses_ont_une_meneuse_a_prevenir() {
    let groupes = vec![json!({"id": 1, "leader_id": 1, "zone_ids": [1, 2, 3]})];
    assert_eq!(meneuses_de(&groupes, 2), vec![1]);
    assert_eq!(meneuses_de(&groupes, 3), vec![1]);
    assert!(
        meneuses_de(&groupes, 1).is_empty(),
        "la meneuse passe déjà par le relais"
    );
    assert!(
        meneuses_de(&groupes, 9).is_empty(),
        "zone hors groupe : rien"
    );
}

#[test]
fn la_meneuse_recoit_le_geste_ou_aligne_selon_son_etat() {
    use GesteDUnMembre as G;
    use PourLaMeneuse as M;
    assert_eq!(
        pour_la_meneuse(G::Pause, PlayState::Playing),
        M::Transmettre
    );
    assert_eq!(pour_la_meneuse(G::Pause, PlayState::Paused), M::Aligner);
    assert_eq!(pour_la_meneuse(G::Pause, PlayState::Stopped), M::Rien);
    assert_eq!(
        pour_la_meneuse(G::Reprise, PlayState::Paused),
        M::Transmettre
    );
    assert_eq!(pour_la_meneuse(G::Reprise, PlayState::Playing), M::Aligner);
    assert_eq!(
        pour_la_meneuse(G::Reprise, PlayState::Stopped),
        M::PrendreLaTete
    );
    assert_eq!(
        pour_la_meneuse(G::Arret, PlayState::Playing),
        M::Transmettre
    );
    assert_eq!(pour_la_meneuse(G::Arret, PlayState::Paused), M::Transmettre);
    assert_eq!(pour_la_meneuse(G::Arret, PlayState::Stopped), M::Aligner);
}

#[test]
fn le_maitre_garde_les_decalages_enregistres_et_saute_le_volume_fixe() {
    let decalages = json!({"2": -0.1, "3": 0.8}).as_object().cloned().unwrap();
    let v = crate::routes::zones::volumes_des_membres(&[1, 2, 3, 4], 0.4, &decalages, &[4]);
    assert_eq!(
        v.len(),
        3,
        "le membre en volume fixe ne reçoit rien : {v:?}"
    );
    assert_eq!(v[0], (1, 0.4));
    assert!((v[1].1 - 0.3).abs() < 1e-9, "{v:?}");
    assert_eq!(v[2], (3, 1.0), "borné à 100 %");
}

// --- De bout en bout ----------------------------------------------------------

struct Banc {
    state: AppState,
    app: axum::Router,
    a: i64,
    b: i64,
    c: i64,
    d: i64,
}

const GROUPE: i64 = 7;

async fn banc() -> Banc {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let repo = ZoneRepo::with_backend(state.backend.clone());
    let mut ids = Vec::new();
    for nom in ["A", "B", "C", "D"] {
        let appareil = format!("uuid:8445-{nom}");
        let id = repo.create(nom, Some("dlna"), Some(&appareil)).unwrap();
        repo.update_volume(id, 50.0).unwrap();
        state
            .outputs
            .lock()
            .await
            .register(Box::new(MockOutput::new(&appareil, nom).with_type("dlna")));
        ids.push(id);
    }
    let (a, b, c, d) = (ids[0], ids[1], ids[2], ids[3]);
    SettingsRepo::with_backend(state.backend.clone())
        .set(
            "zone_groups",
            &json!([{"id": GROUPE, "group_id": GROUPE.to_string(), "name": "Salon",
                     "leader_id": a, "zone_ids": [a, b, c]}])
            .to_string(),
        )
        .unwrap();
    crate::startup::restore_zone_volumes(&state).await;
    lancer_le_relais_de_groupe(&state);
    let app = crate::routes::router(state.clone());
    Banc {
        state,
        app,
        a,
        b,
        c,
        d,
    }
}

impl Banc {
    async fn jouer(&self, zone: i64) {
        self.state
            .playback
            .play(
                zone,
                NowPlaying {
                    title: "Titre".into(),
                    source: "local".into(),
                    track_id: Some(42),
                    duration_ms: 240_000,
                    ..Default::default()
                },
            )
            .await;
    }

    /// Les `started` émis par `jouer` passent d'abord par le relais (la
    /// meneuse A « aligne » B et C, déjà sur sa piste : rien) ; sans cette
    /// attente, un `started` de A traité APRÈS la pause de B la ferait
    /// reprendre — un artefact du banc, pas un relais du geste.
    async fn laisser_le_relais_se_poser(&self) {
        tokio::time::sleep(Duration::from_millis(400)).await;
    }

    async fn etat(&self, zone: i64) -> PlayState {
        self.state.playback.get_state(zone).await.state
    }

    async fn poster(&self, chemin: &str, corps: Option<Value>) -> StatusCode {
        let requete = Request::post(chemin).header("content-type", "application/json");
        let requete = match corps {
            Some(c) => requete.body(Body::from(c.to_string())),
            None => requete.body(Body::empty()),
        }
        .unwrap();
        self.app.clone().oneshot(requete).await.unwrap().status()
    }

    async fn mock<R>(&self, zone: &str, lire: impl FnOnce(&MockOutput) -> R) -> R {
        let registre = self.state.outputs.lock().await;
        let sortie = registre.get(&format!("uuid:8445-{zone}")).unwrap();
        let garde = sortie.lock().await;
        lire(garde.as_any().downcast_ref::<MockOutput>().unwrap())
    }

    async fn volumes_recus(&self, zone: &str) -> Vec<f64> {
        let registre = self.state.outputs.lock().await;
        let sortie = registre.get(&format!("uuid:8445-{zone}")).unwrap();
        let garde = sortie.lock().await;
        garde
            .as_any()
            .downcast_ref::<MockOutput>()
            .unwrap()
            .volume_calls()
            .await
    }

    fn groupes(&self) -> String {
        SettingsRepo::with_backend(self.state.backend.clone())
            .get("zone_groups")
            .unwrap()
            .unwrap()
    }
}

/// Attend (au plus 5 s) que les zones soient dans l'état voulu : le relais
/// agit dans des tâches détachées, et l'arrêt attend `DELAI_AVANT_D_ARRETER`.
async fn attendre(banc: &Banc, zones: &[i64], voulu: PlayState) -> bool {
    for _ in 0..250 {
        let mut toutes = true;
        for z in zones {
            toutes &= banc.etat(*z).await == voulu;
        }
        if toutes {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    false
}

#[tokio::test]
async fn une_pause_sur_une_suiveuse_met_tout_le_groupe_en_pause() {
    let banc = banc().await;
    for z in [banc.a, banc.b, banc.c, banc.d] {
        banc.jouer(z).await;
    }
    banc.laisser_le_relais_se_poser().await;
    let avant = banc.groupes();

    let statut = banc
        .poster(&format!("/api/v1/zones/{}/pause", banc.b), None)
        .await;
    assert_eq!(statut, StatusCode::OK);

    assert!(
        attendre(&banc, &[banc.a, banc.b, banc.c], PlayState::Paused).await,
        "pause sur B (suiveuse) : A {:?}, B {:?}, C {:?}",
        banc.etat(banc.a).await,
        banc.etat(banc.b).await,
        banc.etat(banc.c).await,
    );
    assert_eq!(
        banc.etat(banc.d).await,
        PlayState::Playing,
        "D, hors groupe, joue toujours"
    );
    assert_eq!(
        banc.groupes(),
        avant,
        "la meneuse ne change pas sur une pause"
    );

    // Reprise sur C, l'autre suiveuse : tout le groupe repart.
    let statut = banc
        .poster(&format!("/api/v1/zones/{}/resume", banc.c), None)
        .await;
    assert_eq!(statut, StatusCode::OK);
    assert!(
        attendre(&banc, &[banc.a, banc.b, banc.c], PlayState::Playing).await,
        "reprise sur C : A {:?}, B {:?}",
        banc.etat(banc.a).await,
        banc.etat(banc.b).await,
    );
}

#[tokio::test]
async fn un_arret_sur_une_suiveuse_arrete_le_groupe_sans_le_defaire() {
    let banc = banc().await;
    for z in [banc.a, banc.b, banc.c, banc.d] {
        banc.jouer(z).await;
    }
    banc.laisser_le_relais_se_poser().await;
    let avant = banc.groupes();

    let statut = banc
        .poster(&format!("/api/v1/zones/{}/stop", banc.c), None)
        .await;
    assert_eq!(statut, StatusCode::OK);

    assert!(
        attendre(&banc, &[banc.a, banc.b, banc.c], PlayState::Stopped).await,
        "arrêt sur C : A {:?}, B {:?}",
        banc.etat(banc.a).await,
        banc.etat(banc.b).await,
    );
    assert_eq!(
        banc.etat(banc.d).await,
        PlayState::Playing,
        "D, hors groupe, joue toujours"
    );
    assert_eq!(
        banc.groupes(),
        avant,
        "un arrêt de groupe ne défait pas le groupe"
    );
}

/// Aucune boucle : une fois le groupe en pause, plus rien ne bouge — pas de
/// reprise, pas d'arrêt, pas de nouvelle lecture renvoyés d'une zone à l'autre.
#[tokio::test]
async fn le_relais_ne_boucle_pas() {
    let banc = banc().await;
    for z in [banc.a, banc.b, banc.c] {
        banc.jouer(z).await;
    }
    banc.laisser_le_relais_se_poser().await;
    banc.poster(&format!("/api/v1/zones/{}/pause", banc.b), None)
        .await;
    assert!(attendre(&banc, &[banc.a, banc.b, banc.c], PlayState::Paused).await);
    // Laisser au relais le temps de « répondre » s'il le devait (au-delà du
    // délai d'arrêt).
    tokio::time::sleep(DELAI_AVANT_D_ARRETER + Duration::from_millis(500)).await;
    for (nom, z) in [("A", banc.a), ("B", banc.b), ("C", banc.c)] {
        assert_eq!(
            banc.etat(z).await,
            PlayState::Paused,
            "{nom} est resté en pause"
        );
        assert_eq!(
            banc.mock(nom, |m| m.resume_call_count()).await,
            0,
            "{nom} : aucune reprise"
        );
        assert_eq!(
            banc.mock(nom, |m| m.stop_call_count()).await,
            0,
            "{nom} : aucun arrêt"
        );
    }
    for nom in ["A", "B", "C"] {
        let lectures = {
            let registre = banc.state.outputs.lock().await;
            let sortie = registre.get(&format!("uuid:8445-{nom}")).unwrap();
            let garde = sortie.lock().await;
            garde
                .as_any()
                .downcast_ref::<MockOutput>()
                .unwrap()
                .play_call_count()
                .await
        };
        assert_eq!(lectures, 0, "{nom} : aucune lecture relancée");
    }
}

/// Zone hors groupe : ses gestes ne valent que pour elle, comme avant.
#[tokio::test]
async fn une_zone_hors_groupe_ne_touche_pas_au_groupe() {
    let banc = banc().await;
    for z in [banc.a, banc.b, banc.c, banc.d] {
        banc.jouer(z).await;
    }
    banc.laisser_le_relais_se_poser().await;
    banc.poster(&format!("/api/v1/zones/{}/pause", banc.d), None)
        .await;
    assert!(attendre(&banc, &[banc.d], PlayState::Paused).await);
    tokio::time::sleep(Duration::from_millis(300)).await;
    for z in [banc.a, banc.b, banc.c] {
        assert_eq!(banc.etat(z).await, PlayState::Playing);
    }
}

#[tokio::test]
async fn le_volume_maitre_arrive_a_chaque_membre_sauf_au_volume_fixe() {
    let banc = banc().await;
    ZoneRepo::with_backend(banc.state.backend.clone())
        .update_fixed_volume(banc.c, true)
        .unwrap();
    // Décalage enregistré au préalable pour B (−10 points).
    let statut = banc
        .poster(
            &format!("/api/v1/zone-manager/groups/{GROUPE}/volume"),
            Some(json!({"master_volume": 0.5, "offsets": {banc.b.to_string(): -0.1}})),
        )
        .await;
    assert_eq!(statut, StatusCode::OK);

    // Le curseur du client web n'envoie QUE le maître.
    let statut = banc
        .poster(
            &format!("/api/v1/zone-manager/groups/{GROUPE}/volume"),
            Some(json!({"master_volume": 0.4})),
        )
        .await;
    assert_eq!(statut, StatusCode::OK);

    let a = banc.volumes_recus("A").await;
    let b = banc.volumes_recus("B").await;
    let c = banc.volumes_recus("C").await;
    let d = banc.volumes_recus("D").await;
    assert!(
        a.last().is_some_and(|v| (v - 0.4).abs() < 1e-6),
        "A reçoit le maître : {a:?}"
    );
    assert!(
        b.last().is_some_and(|v| (v - 0.3).abs() < 1e-6),
        "B garde son décalage enregistré : {b:?}"
    );
    assert!(
        c.is_empty(),
        "C, en volume fixe, ne reçoit aucun volume : {c:?}"
    );
    assert!(d.is_empty(), "D, hors groupe, n'est pas touchée : {d:?}");
    // La route historique `/zones/groups/{id}/volume` prend le même chemin.
    let statut = banc
        .poster(
            &format!("/api/v1/zones/groups/{GROUPE}/volume"),
            Some(json!({"master_volume": 0.45})),
        )
        .await;
    assert_eq!(statut, StatusCode::OK);
    let b = banc.volumes_recus("B").await;
    assert!(b.last().is_some_and(|v| (v - 0.35).abs() < 1e-6), "{b:?}");
    assert!(banc.volumes_recus("C").await.is_empty());
}
