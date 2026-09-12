//! Une consigne en dB doit avoir un endroit où arriver (#1274).
//!
//! `zaurux`, forum-hifi n°41831 : « pas de réglage au db près ». Le contrat
//! HTTP existe depuis #2885 (`volume_db` en lecture et en écriture) et la
//! colonne ne l'arrondit plus depuis #2886. Restait le fil.
//!
//! ## Le défaut que ce fichier cloue
//!
//! `can_set_volume` dit qu'une sortie sait régler le volume. Il ne dit pas
//! *avec quelle finesse*. Sept des treize sorties intégrées — DLNA/UPnP,
//! OpenHome, BluOS, Squeezebox, HQPlayer et les deux OAAT — n'envoient au
//! périphérique qu'un **entier 0..100**. Sur cette grille :
//!
//! - le pas vaut 0,09 dB sous la pleine échelle, mais **6,02 dB** entre 1 %
//!   et 2 % ;
//! - au-dessous de 1 % (−40 dB) il n'y a plus rien : `round(0,316) = 0`.
//!
//! Demander −50 dB à une telle zone n'était donc pas imprécis, c'était
//! **muet** : le périphérique recevait zéro, le serveur gardait le `f64`
//! exact, le persistait et répondait un succès. La zone était annoncée à
//! −50 dB et ne faisait aucun bruit — indiscernable d'un mute volontaire.
//! C'est le défaut que #2886 avait corrigé dans `zones.volume` et laissé
//! entier sur le fil.
//!
//! ## Ce que les tests exigent
//!
//! Jamais un code HTTP seul — le « 200 pour rien » est le faux vert le plus
//! fréquent ici. Chaque refus est vérifié sur **trois** faits : le code, le
//! motif nommé (la grille ET le plancher chiffré), et le fait que le
//! périphérique n'a **rien reçu**. Chaque acceptation vérifie symétriquement
//! que le périphérique a bien reçu le bon niveau.
//!
//! ⚠️ `tune-server` porte `autotests = false` — ce fichier n'est compilé que
//! parce qu'il est déclaré dans l'agrégateur `server_contracts.rs`.

use std::sync::Mutex;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_core::outputs::{OutputCapabilities, OutputStatus, OutputTarget, TransportState};

/// Sortie d'essai dont on choisit la grille et qui **accepte** tout.
///
/// Accepter est le point : sans cela, le rouge d'avant le correctif viendrait
/// d'un échec de périphérique et non du défaut visé. Ici le backend dit
/// toujours oui, donc un `volume_db` hors grille qui arrive jusqu'à lui
/// produit exactement l'ancien comportement — un succès sur un silence.
struct SortieDEssai {
    device_id: &'static str,
    capabilities: OutputCapabilities,
    /// Tous les niveaux réellement reçus, dans l'ordre.
    recus: Mutex<Vec<f64>>,
}

impl SortieDEssai {
    fn au_pour_cent(device_id: &'static str) -> Self {
        Self {
            device_id,
            capabilities: OutputCapabilities::v1(true, true, true, true, true, false)
                .with_percent_volume(),
            recus: Mutex::new(Vec::new()),
        }
    }

    fn continue_(device_id: &'static str) -> Self {
        Self {
            device_id,
            // Aucune grille déclarée : c'est le défaut du champ, et le témoin.
            capabilities: OutputCapabilities::v1(true, true, true, true, true, false),
            recus: Mutex::new(Vec::new()),
        }
    }
}

#[async_trait::async_trait]
impl OutputTarget for SortieDEssai {
    fn name(&self) -> &str {
        "Sortie d'essai"
    }
    fn device_id(&self) -> &str {
        self.device_id
    }
    fn output_type(&self) -> &str {
        "essai"
    }
    fn capabilities(&self) -> OutputCapabilities {
        self.capabilities.clone()
    }
    /// Sans cette redéfinition, le défaut du trait rend `&()` et la relecture
    /// des niveaux reçus — la seule preuve qui ne soit pas un code HTTP —
    /// serait impossible.
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    async fn pause(&self) -> Result<(), String> {
        Ok(())
    }
    async fn resume(&self) -> Result<(), String> {
        Ok(())
    }
    async fn stop(&self) -> Result<(), String> {
        Ok(())
    }
    async fn seek(&self, _position_ms: u64) -> Result<(), String> {
        Ok(())
    }
    async fn set_volume(&self, volume: f64) -> Result<(), String> {
        self.recus.lock().unwrap().push(volume);
        Ok(())
    }
    async fn set_mute(&self, _muted: bool) -> Result<(), String> {
        Ok(())
    }
    async fn get_status(&self) -> Result<OutputStatus, String> {
        Ok(OutputStatus {
            state: TransportState::Stopped,
            ..Default::default()
        })
    }
    async fn is_available(&self) -> bool {
        true
    }
}

struct Banc {
    app: axum::Router,
    state: tune_server::state::AppState,
}

impl Banc {
    async fn neuf() -> Self {
        let state = tune_server::state::AppState::new(":memory:", 0, Default::default()).unwrap();
        let app = tune_server::routes::router(state.clone());
        Self { app, state }
    }

    /// Enregistre la sortie et rend l'identifiant d'une zone qui lui est liée.
    async fn zone_liee(&self, sortie: SortieDEssai) -> i64 {
        let device_id = sortie.device_id.to_string();
        self.state.outputs.lock().await.register(Box::new(sortie));
        let id = self.zone().await;
        let (status, corps) = self
            .envoyer(
                "PATCH",
                &format!("/api/v1/zones/{id}"),
                json!({ "output_device_id": device_id }),
            )
            .await;
        assert!(
            status.is_success(),
            "liaison de la zone à {device_id} : {status} — {corps}"
        );
        id
    }

    async fn zone(&self) -> i64 {
        let (status, corps) = self
            .envoyer("POST", "/api/v1/zones", json!({"name": "Salon"}))
            .await;
        assert_eq!(status, StatusCode::CREATED, "création de zone : {corps}");
        json_de(&corps)["id"].as_i64().expect("un id de zone")
    }

    /// Rend le corps **brut** : les refus du PATCH sont du texte, ceux des
    /// routes de volume du JSON. Un test qui n'accepterait qu'une des deux
    /// formes laisserait l'autre porte sans preuve.
    async fn envoyer(&self, methode: &str, path: &str, body: Value) -> (StatusCode, String) {
        let requete = Request::builder()
            .method(methode)
            .uri(path)
            .header("Content-Type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        self.jouer(requete).await
    }

    async fn get(&self, path: &str) -> (StatusCode, String) {
        self.jouer(Request::get(path).body(Body::empty()).unwrap())
            .await
    }

    async fn jouer(&self, requete: Request<Body>) -> (StatusCode, String) {
        let resp = self.app.clone().oneshot(requete).await.unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, String::from_utf8_lossy(&bytes).into_owned())
    }

    /// Ce que le périphérique a REÇU — la seule preuve qui ne soit pas un
    /// code HTTP.
    async fn recus(&self, device_id: &str) -> Vec<f64> {
        let sortie = self
            .state
            .outputs
            .lock()
            .await
            .get(device_id)
            .expect("la sortie d'essai est enregistrée");
        let garde = sortie.lock().await;
        garde
            .as_any()
            .downcast_ref::<SortieDEssai>()
            .expect("notre sortie d'essai")
            .recus
            .lock()
            .unwrap()
            .clone()
    }
}

fn json_de(corps: &str) -> Value {
    serde_json::from_str(corps).unwrap_or(json!(null))
}

/// Le motif doit NOMMER la grille et le plancher. Un 400 sans chiffre
/// n'apprend rien à qui doit corriger sa demande — et « le 200 pour rien » a
/// un jumeau, « le 400 pour rien ».
fn motif_nomme_la_grille(corps: &str) {
    assert!(
        corps.contains("100 pas"),
        "le motif doit nommer la grille — {corps}"
    );
    assert!(
        corps.contains("-40.0 dB"),
        "le motif doit nommer le plancher atteignable — {corps}"
    );
}

/// ROUGE avant le correctif : le serveur répondait un succès et le
/// périphérique recevait zéro.
#[tokio::test]
async fn une_consigne_hors_grille_est_refusee_et_n_atteint_pas_le_peripherique() {
    let banc = Banc::neuf().await;
    let id = banc
        .zone_liee(SortieDEssai::au_pour_cent("essai-pct"))
        .await;

    let (status, corps) = banc
        .envoyer(
            "POST",
            &format!("/api/v1/zones/{id}/volume"),
            json!({ "volume_db": -50.0 }),
        )
        .await;

    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "−50 dB sur une grille de 1 % vaut round(0,316) = 0 : le refus doit \
         être explicite, pas un succès sur un silence — {corps}"
    );
    assert_eq!(
        json_de(&corps)["error"],
        "volume_db_hors_resolution",
        "le refus doit porter un nom stable — {corps}"
    );
    motif_nomme_la_grille(&corps);

    // La preuve qui n'est pas un code : rien n'est parti sur le fil.
    assert!(
        banc.recus("essai-pct").await.is_empty(),
        "aucun niveau ne doit atteindre un périphérique qui ne peut pas le tenir"
    );
}

/// Les TROIS portes d'écriture du volume refusent, pas seulement la première
/// corrigée. Une seule porte laissée ouverte suffit à ramener le silence.
#[tokio::test]
async fn les_trois_portes_d_ecriture_refusent_la_meme_consigne() {
    for (methode, suffixe) in [("POST", "/volume"), ("PUT", "/volume"), ("PATCH", "")] {
        let banc = Banc::neuf().await;
        let id = banc
            .zone_liee(SortieDEssai::au_pour_cent("essai-pct"))
            .await;
        let chemin = format!("/api/v1/zones/{id}{suffixe}");

        let (status, corps) = banc
            .envoyer(methode, &chemin, json!({ "volume_db": -50.0 }))
            .await;

        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "{methode} {chemin} doit refuser −50 dB — {corps}"
        );
        motif_nomme_la_grille(&corps);
        assert!(
            banc.recus("essai-pct").await.is_empty(),
            "{methode} {chemin} : rien ne doit partir sur le fil"
        );
    }
}

/// TÉMOIN — vert avant comme après. Le garde-fou ne doit pas rendre le
/// réglage en dB inutilisable là où la sortie suit : −18 dB, la cible même de
/// l'issue, passe et ATTEINT le périphérique.
#[tokio::test]
async fn temoin_une_consigne_dans_la_grille_passe_et_atteint_le_peripherique() {
    let banc = Banc::neuf().await;
    let id = banc
        .zone_liee(SortieDEssai::au_pour_cent("essai-pct"))
        .await;

    for db in [-18.0, -40.0, -6.0] {
        let (status, corps) = banc
            .envoyer(
                "POST",
                &format!("/api/v1/zones/{id}/volume"),
                json!({ "volume_db": db }),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{db} dB doit passer — {corps}");
    }

    let recus = banc.recus("essai-pct").await;
    assert_eq!(
        recus.len(),
        3,
        "les trois consignes doivent atteindre le fil — {recus:?}"
    );
    // −18 dB = 0,12589…, et c'est bien ce niveau-là qui part.
    assert!(
        (recus[0] - 10f64.powf(-18.0 / 20.0)).abs() < 1e-9,
        "reçu {recus:?}"
    );
    // −40 dB est le plancher ANNONCÉ : il doit être accepté par le contrôle
    // qui l'annonce, sinon le message ment.
    assert!((recus[1] - 0.01).abs() < 1e-9, "reçu {recus:?}");
}

/// TÉMOIN — le refus est CIBLÉ. La même consigne, sur une sortie au réglage
/// continu, passe : ce n'est pas le dB qu'on refuse, c'est la grille qui ne
/// sait pas le tenir.
#[tokio::test]
async fn temoin_la_meme_consigne_passe_sur_une_sortie_au_reglage_continu() {
    let banc = Banc::neuf().await;
    let id = banc
        .zone_liee(SortieDEssai::continue_("essai-continu"))
        .await;

    let (status, corps) = banc
        .envoyer(
            "POST",
            &format!("/api/v1/zones/{id}/volume"),
            json!({ "volume_db": -50.0 }),
        )
        .await;

    assert_eq!(
        status,
        StatusCode::OK,
        "une sortie sans grille tient −50 dB — {corps}"
    );
    let recus = banc.recus("essai-continu").await;
    assert_eq!(recus.len(), 1, "la consigne doit atteindre le fil");
    assert!(
        (recus[0] - 10f64.powf(-50.0 / 20.0)).abs() < 1e-12,
        "et sans être quantifiée — reçu {recus:?}"
    );
}

/// TÉMOIN — l'aller-retour reste stable bien au-delà du dixième de dB :
/// régler −12,0 dB puis relire rend −12,0 dB, pas −11,97. Vert des deux
/// côtés ; c'est l'acquis de #2885/#2886 que ce lot ne doit pas abîmer.
#[tokio::test]
async fn temoin_l_aller_retour_en_db_reste_exact() {
    let banc = Banc::neuf().await;
    let id = banc
        .zone_liee(SortieDEssai::au_pour_cent("essai-pct"))
        .await;

    for cible in [-12.0, -18.0, -6.5, -0.3, -39.9] {
        let (status, corps) = banc
            .envoyer(
                "POST",
                &format!("/api/v1/zones/{id}/volume"),
                json!({ "volume_db": cible }),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{cible} dB : {corps}");
        assert!(
            (json_de(&corps)["volume_db"].as_f64().expect("volume_db") - cible).abs() < 1e-9,
            "la réponse doit confirmer la cible — {corps}"
        );

        // Et la relecture, y compris la vue persistée, rend le même nombre.
        for chemin in [
            format!("/api/v1/zones/{id}"),
            format!("/api/v1/zones/{id}/status"),
        ] {
            let (_, relu) = banc.get(&chemin).await;
            let relu_db = json_de(&relu)["volume_db"]
                .as_f64()
                .unwrap_or_else(|| panic!("{chemin} : volume_db absent — {relu}"));
            assert!(
                (relu_db - cible).abs() < 1e-9,
                "{chemin} : {cible} dB relu {relu_db} dB"
            );
        }
    }
}

/// La grille est PUBLIÉE : sans elle, un client ne peut pas savoir qu'il ne
/// doit pas proposer −50 dB sur cette zone, et le refus arrive trop tard —
/// après le geste, au lieu de l'empêcher.
#[tokio::test]
async fn la_grille_de_la_sortie_est_visible_dans_la_charge_utile_de_zone() {
    let banc = Banc::neuf().await;
    let id = banc
        .zone_liee(SortieDEssai::au_pour_cent("essai-pct"))
        .await;

    let (status, corps) = banc.get(&format!("/api/v1/zones/{id}")).await;
    assert_eq!(status, StatusCode::OK, "{corps}");
    let resolution = json_de(&corps)["output_capabilities"]["volume_resolution"].clone();
    assert_eq!(
        resolution["kind"], "linear",
        "la zone doit publier la grille de sa sortie — {corps}"
    );
    assert_eq!(resolution["steps"], 100, "{corps}");
}
