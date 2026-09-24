//! Les routes du greffon, montées par l'hôte sous `/api/v1/ext/cd`.
//!
//! * `GET  /etat`   — le lecteur et le disque ;
//! * `GET  /disque` — la TOC, l'identifiant et les métadonnées ;
//! * `POST /jouer`  — `{ "zone_id": 3, "piste": 5 }` : pose le disque entier
//!   en file sur la zone et joue la piste demandée (la 1ʳᵉ sans `piste`).

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::discid::disc_id;
use crate::ejection::ZonesDuDisque;
use crate::fournisseur::source_id;
use crate::hote::{ElementFile, HoteLecture};
use crate::lecteur::{ErreurCd, LecteurDisque, Presence, plateforme_prise_en_charge};
use crate::musicbrainz::{Consultation, InfosDisque};
use crate::toc::Toc;

#[derive(Clone)]
pub struct EtatRoutes {
    pub lecteur: Option<Arc<dyn LecteurDisque>>,
    pub hote: Arc<dyn HoteLecture>,
    pub consultation: Arc<dyn Consultation>,
    pub zones: ZonesDuDisque,
}

pub fn router(etat: EtatRoutes) -> Router<()> {
    Router::new()
        .route("/etat", get(etat_du_lecteur))
        .route("/disque", get(disque))
        .route("/jouer", post(jouer))
        .with_state(etat)
}

fn refus(code: StatusCode, motif: &str, message: String) -> Response {
    (code, Json(json!({ "error": motif, "message": message }))).into_response()
}

/// La TOC du disque inséré, ou la réponse qui dit pourquoi il n'y en a pas.
async fn toc_ou_refus(etat: &EtatRoutes) -> Result<Toc, Box<Response>> {
    let Some(lecteur) = etat.lecteur.clone() else {
        return Err(Box::new(refus(
            StatusCode::NOT_FOUND,
            "aucun_lecteur",
            "Aucun lecteur de CD sur la machine qui fait tourner Tune.".into(),
        )));
    };
    let r = match tokio::task::spawn_blocking(move || lecteur.lire_toc()).await {
        Ok(Ok(toc)) => return Ok(toc),
        Ok(Err(ErreurCd::AucunDisque)) => Err(refus(
            StatusCode::CONFLICT,
            "aucun_disque",
            "Le lecteur est vide.".into(),
        )),
        Ok(Err(e)) => Err(refus(StatusCode::BAD_GATEWAY, "lecture_toc", e.to_string())),
        Err(e) => Err(refus(
            StatusCode::INTERNAL_SERVER_ERROR,
            "interne",
            e.to_string(),
        )),
    };
    r.map_err(Box::new)
}

async fn etat_du_lecteur(State(etat): State<EtatRoutes>) -> Json<Value> {
    let (chemin, presence) = match etat.lecteur.clone() {
        Some(l) => {
            let chemin = l.chemin();
            let presence = tokio::task::spawn_blocking(move || l.presence())
                .await
                .unwrap_or(Presence::AucunLecteur);
            (Some(chemin), presence)
        }
        None => (None, Presence::AucunLecteur),
    };
    Json(json!({
        "plateforme_prise_en_charge": plateforme_prise_en_charge(),
        "lecteur": chemin,
        "presence": presence,
    }))
}

/// Les pistes audio de la TOC, nommées par MusicBrainz ou « Piste N ».
fn elements(toc: &Toc, disc: &str, infos: Option<&InfosDisque>) -> Vec<ElementFile> {
    toc.pistes_audio()
        .map(|p| {
            let mb = infos.and_then(|i| i.pistes.get(&p.numero));
            ElementFile {
                source_id: source_id(disc, p.numero),
                titre: mb
                    .map(|m| m.titre.clone())
                    .unwrap_or_else(|| format!("Piste {}", p.numero)),
                artiste: mb
                    .and_then(|m| m.artiste.clone())
                    .or_else(|| infos.map(|i| i.artiste.clone()).filter(|a| !a.is_empty()))
                    .unwrap_or_default(),
                album: infos.map(|i| i.titre.clone()).filter(|t| !t.is_empty()),
                pochette: infos.and_then(|i| i.pochette.clone()),
                duree_ms: toc.duree_ms(p.numero).unwrap_or(0) as i64,
                numero: p.numero,
            }
        })
        .collect()
}

async fn disque(State(etat): State<EtatRoutes>) -> Response {
    let toc = match toc_ou_refus(&etat).await {
        Ok(v) => v,
        Err(r) => return *r,
    };
    let disc = disc_id(&toc);
    let infos = etat.consultation.consulter(&disc).await;
    let pistes: Vec<Value> = elements(&toc, &disc, infos.as_ref())
        .into_iter()
        .map(|e| {
            let p = toc.piste(e.numero);
            json!({
                "numero": e.numero,
                "titre": e.titre,
                "artiste": e.artiste,
                "duree_ms": e.duree_ms,
                "premier_secteur": p.map(|p| p.debut),
                "secteurs": toc.secteurs(e.numero),
                "source_id": e.source_id,
            })
        })
        .collect();
    Json(json!({
        "disc_id": disc,
        "metadonnees": if infos.is_some() { "musicbrainz" } else { "repli" },
        "titre": infos.as_ref().map(|i| i.titre.clone()),
        "artiste": infos.as_ref().map(|i| i.artiste.clone()),
        "release_id": infos.as_ref().and_then(|i| i.release_id.clone()),
        "pochette": infos.as_ref().and_then(|i| i.pochette.clone()),
        "toc": {
            "premiere": toc.premiere,
            "derniere": toc.derniere,
            "fin": toc.fin,
            "pistes_de_donnees": toc.pistes.iter().filter(|p| !p.audio).map(|p| p.numero).collect::<Vec<_>>(),
        },
        "pistes": pistes,
    }))
    .into_response()
}

#[derive(Deserialize)]
struct DemandeJouer {
    zone_id: i64,
    piste: Option<u8>,
}

async fn jouer(State(etat): State<EtatRoutes>, Json(d): Json<DemandeJouer>) -> Response {
    let toc = match toc_ou_refus(&etat).await {
        Ok(v) => v,
        Err(r) => return *r,
    };
    let disc = disc_id(&toc);
    let infos = etat.consultation.consulter(&disc).await;
    let file = elements(&toc, &disc, infos.as_ref());
    let numero = d
        .piste
        .unwrap_or_else(|| file.first().map(|e| e.numero).unwrap_or(1));
    let Some(depart) = file.iter().position(|e| e.numero == numero) else {
        return refus(
            StatusCode::BAD_REQUEST,
            "piste_inconnue",
            format!("La piste {numero} n'est pas une piste audio de ce disque."),
        );
    };
    let longueur = file.len();
    match etat.hote.jouer_file(d.zone_id, file, depart).await {
        Ok(()) => {
            etat.zones.lock().await.insert(d.zone_id);
            Json(json!({
                "zone_id": d.zone_id,
                "disc_id": disc,
                "piste": numero,
                "file": longueur,
            }))
            .into_response()
        }
        Err(e) => refus(StatusCode::BAD_GATEWAY, "lecture", e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discid::tests::{ATTENDU, toc_du_vecteur};
    use crate::ejection::tests::HoteTemoin;
    use crate::simule::LecteurSimule;
    use async_trait::async_trait;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    struct SansReseau;
    #[async_trait]
    impl Consultation for SansReseau {
        async fn consulter(&self, _: &str) -> Option<InfosDisque> {
            None
        }
    }

    struct Fixture;
    #[async_trait]
    impl Consultation for Fixture {
        async fn consulter(&self, disc: &str) -> Option<InfosDisque> {
            let v: Value = serde_json::from_str(include_str!(
                "../tests/fixtures/discid_Wn8eRBtfLDfM0qjYPdxrz.Zjs_U-.json"
            ))
            .unwrap();
            crate::musicbrainz::lire_reponse(&v, disc)
        }
    }

    fn etat(
        lecteur: Option<Arc<dyn LecteurDisque>>,
        c: Arc<dyn Consultation>,
    ) -> (EtatRoutes, Arc<HoteTemoin>) {
        let hote = Arc::new(HoteTemoin::default());
        (
            EtatRoutes {
                lecteur,
                hote: hote.clone(),
                consultation: c,
                zones: Arc::default(),
            },
            hote,
        )
    }

    async fn appel(
        r: Router<()>,
        methode: &str,
        uri: &str,
        corps: Option<Value>,
    ) -> (StatusCode, Value) {
        let req = Request::builder().method(methode).uri(uri);
        let req = match corps {
            Some(c) => req
                .header("content-type", "application/json")
                .body(Body::from(c.to_string()))
                .unwrap(),
            None => req.body(Body::empty()).unwrap(),
        };
        let rep = r.oneshot(req).await.unwrap();
        let code = rep.status();
        let octets = axum::body::to_bytes(rep.into_body(), 1 << 20)
            .await
            .unwrap();
        (code, serde_json::from_slice(&octets).unwrap())
    }

    fn simule() -> Option<Arc<dyn LecteurDisque>> {
        Some(Arc::new(LecteurSimule::new(toc_du_vecteur())))
    }

    /// Témoin 8 — les routes rendent la bonne forme.
    #[tokio::test]
    async fn etat_rend_le_lecteur_et_la_presence() {
        let (e, _) = etat(simule(), Arc::new(SansReseau));
        let (code, v) = appel(router(e), "GET", "/etat", None).await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(v["presence"], "disque");
        assert_eq!(v["lecteur"], "simulé");
        assert!(v["plateforme_prise_en_charge"].is_boolean());

        let (e, _) = etat(None, Arc::new(SansReseau));
        let (_, v) = appel(router(e), "GET", "/etat", None).await;
        assert_eq!(v["presence"], "aucun_lecteur");
        assert!(v["lecteur"].is_null());
    }

    #[tokio::test]
    async fn disque_sans_reseau_nomme_les_pistes_piste_n() {
        let (e, _) = etat(simule(), Arc::new(SansReseau));
        let (code, v) = appel(router(e), "GET", "/disque", None).await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(v["disc_id"], ATTENDU);
        assert_eq!(v["metadonnees"], "repli");
        assert!(v["titre"].is_null());
        assert_eq!(v["toc"]["premiere"], 1);
        assert_eq!(v["toc"]["derniere"], 10);
        let pistes = v["pistes"].as_array().unwrap();
        assert_eq!(pistes.len(), 10);
        assert_eq!(pistes[0]["titre"], "Piste 1");
        assert_eq!(pistes[0]["duree_ms"], 250_013);
        assert_eq!(pistes[0]["premier_secteur"], 0);
        assert_eq!(pistes[0]["secteurs"], 18_751);
        assert_eq!(pistes[1]["source_id"], format!("{ATTENDU}/2"));
    }

    #[tokio::test]
    async fn disque_avec_musicbrainz_nomme_titre_artiste_et_pistes() {
        let (e, _) = etat(simule(), Arc::new(Fixture));
        let (_, v) = appel(router(e), "GET", "/disque", None).await;
        assert_eq!(v["metadonnees"], "musicbrainz");
        assert_eq!(v["titre"], "Fiction");
        assert_eq!(v["artiste"], "Dark Tranquillity");
        assert_eq!(v["pistes"][0]["titre"], "Nothing to No One");
        assert_eq!(v["pistes"][0]["artiste"], "Dark Tranquillity");
    }

    #[tokio::test]
    async fn jouer_pose_le_disque_entier_en_file_et_part_de_la_piste_demandee() {
        let (e, hote) = etat(simule(), Arc::new(Fixture));
        let zones = e.zones.clone();
        let (code, v) = appel(
            router(e),
            "POST",
            "/jouer",
            Some(json!({"zone_id": 3, "piste": 4})),
        )
        .await;
        assert_eq!(code, StatusCode::OK, "{v}");
        assert_eq!(
            v,
            json!({"zone_id": 3, "disc_id": ATTENDU, "piste": 4, "file": 10})
        );
        let files = hote.files.lock().await;
        let (zone, file, depart) = &files[0];
        assert_eq!((*zone, *depart), (3, 3));
        assert_eq!(file.len(), 10);
        assert_eq!(file[3].source_id, format!("{ATTENDU}/4"));
        assert_eq!(file[3].album.as_deref(), Some("Fiction"));
        assert!(zones.lock().await.contains(&3));
    }

    #[tokio::test]
    async fn les_refus_disent_pourquoi() {
        let (e, _) = etat(None, Arc::new(SansReseau));
        let (code, v) = appel(router(e), "GET", "/disque", None).await;
        assert_eq!(code, StatusCode::NOT_FOUND);
        assert_eq!(v["error"], "aucun_lecteur");

        let l = Arc::new(LecteurSimule::new(toc_du_vecteur()));
        l.ejecter();
        let (e, _) = etat(Some(l), Arc::new(SansReseau));
        let (code, v) = appel(router(e), "GET", "/disque", None).await;
        assert_eq!(code, StatusCode::CONFLICT);
        assert_eq!(v["error"], "aucun_disque");

        let (e, _) = etat(simule(), Arc::new(SansReseau));
        let (code, v) = appel(
            router(e),
            "POST",
            "/jouer",
            Some(json!({"zone_id": 1, "piste": 42})),
        )
        .await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
        assert_eq!(v["error"], "piste_inconnue");
    }
}
