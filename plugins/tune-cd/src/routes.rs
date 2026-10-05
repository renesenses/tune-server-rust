//! Les routes du greffon, montées par l'hôte sous `/api/v1/ext/cd`.
//!
//! * `GET  /etat`   — le lecteur et le disque ;
//! * `GET  /disque` — la TOC, l'identifiant et les métadonnées ;
//! * `POST /jouer`  — `{ "zone_id": 3, "piste": 5 }` : pose le disque entier
//!   en file sur la zone et joue la piste demandée (la 1ʳᵉ sans `piste`) ;
//! * `POST /ejecter` — `{ "forcer": true }` facultatif : éjecte le disque du
//!   lecteur courant (fil 2135). Si une zone le joue (ou le tient en pause),
//!   refus `409 lecture_en_cours` qui nomme les zones ; avec `forcer`, ces
//!   zones sont d'abord arrêtées, puis le disque est éjecté.

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::Notify;

use crate::discid::disc_id;
use crate::ejection::ZonesDuDisque;
use crate::fournisseur::{SOURCE, source_id};
use crate::hote::{ElementFile, HoteLecture};
use crate::lecteur::{
    ErreurCd, ErreurEjection, LecteurDisque, Presence, plateforme_prise_en_charge,
};
use crate::musicbrainz::{Consultation, InfosDisque};
use crate::toc::Toc;

#[derive(Clone)]
pub struct EtatRoutes {
    pub lecteur: Option<Arc<dyn LecteurDisque>>,
    pub hote: Arc<dyn HoteLecture>,
    pub consultation: Arc<dyn Consultation>,
    pub zones: ZonesDuDisque,
    /// Réveille la surveillance (`ejection::Surveillant`) : après une
    /// éjection commandée, la source `cd` passe à `vide` et
    /// `sources.changed` part tout de suite, sans attendre le tour suivant.
    pub reveil: Arc<Notify>,
}

pub fn router(etat: EtatRoutes) -> Router<()> {
    Router::new()
        .route("/etat", get(etat_du_lecteur))
        .route("/disque", get(disque))
        .route("/jouer", post(jouer))
        .route("/ejecter", post(ejecter))
        .with_state(etat)
}

fn refus(code: StatusCode, motif: &str, message: String) -> Response {
    (code, Json(json!({ "error": motif, "message": message }))).into_response()
}

/// Un refus : statut, motif stable, message lisible.
pub(crate) type Refus = (StatusCode, &'static str, String);

fn aucun_lecteur() -> Refus {
    (
        StatusCode::NOT_FOUND,
        "aucun_lecteur",
        "Aucun lecteur de CD sur la machine qui fait tourner Tune.".into(),
    )
}

/// La TOC du disque inséré, ou le refus qui dit pourquoi il n'y en a pas.
async fn toc_ou_refus(etat: &EtatRoutes) -> Result<Toc, Refus> {
    let Some(lecteur) = etat.lecteur.clone() else {
        return Err(aucun_lecteur());
    };
    let l = lecteur.clone();
    match tokio::task::spawn_blocking(move || l.lire_toc()).await {
        Ok(Ok(toc)) => Ok(toc),
        // #5161 — le lecteur se branche à chaud : tant qu'il n'est pas là,
        // c'est « aucun lecteur », pas « lecteur vide ».
        Ok(Err(ErreurCd::AucunDisque))
            if tokio::task::spawn_blocking(move || lecteur.presence())
                .await
                .is_ok_and(|p| p == Presence::AucunLecteur) =>
        {
            Err(aucun_lecteur())
        }
        Ok(Err(ErreurCd::AucunDisque)) => Err((
            StatusCode::CONFLICT,
            "aucun_disque",
            "Le lecteur est vide.".into(),
        )),
        Ok(Err(e)) => Err((StatusCode::BAD_GATEWAY, "lecture_toc", e.to_string())),
        Err(e) => Err((StatusCode::INTERNAL_SERVER_ERROR, "interne", e.to_string())),
    }
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
        Err((code, motif, message)) => return refus(code, motif, message),
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
    match jouer_disque(&etat, d.zone_id, d.piste).await {
        Ok(v) => Json(v).into_response(),
        Err((code, motif, message)) => refus(code, motif, message),
    }
}

/// Pose le disque entier en file sur la zone et joue la piste demandée (la
/// 1ʳᵉ sans `piste`). Partagé par `POST /jouer` et par la délégation du
/// registre des sources (`POST /api/v1/sources/cd/jouer`, #5065).
pub(crate) async fn jouer_disque(
    etat: &EtatRoutes,
    zone_id: i64,
    piste: Option<u8>,
) -> Result<Value, Refus> {
    let toc = toc_ou_refus(etat).await?;
    let disc = disc_id(&toc);
    let infos = etat.consultation.consulter(&disc).await;
    let file = elements(&toc, &disc, infos.as_ref());
    let numero = piste.unwrap_or_else(|| file.first().map(|e| e.numero).unwrap_or(1));
    let Some(depart) = file.iter().position(|e| e.numero == numero) else {
        return Err((
            StatusCode::BAD_REQUEST,
            "piste_inconnue",
            format!("La piste {numero} n'est pas une piste audio de ce disque."),
        ));
    };
    let longueur = file.len();
    match etat.hote.jouer_file(zone_id, file, depart).await {
        Ok(()) => {
            etat.zones.lock().await.insert(zone_id);
            Ok(json!({
                "zone_id": zone_id,
                "disc_id": disc,
                "piste": numero,
                "file": longueur,
            }))
        }
        Err(e) => Err((StatusCode::BAD_GATEWAY, "lecture", e)),
    }
}

#[derive(Deserialize, Default)]
struct DemandeEjecter {
    /// Arrêter d'abord les zones qui jouent le disque (le client a demandé
    /// confirmation). Sans lui, une lecture en cours fait refuser.
    #[serde(default)]
    forcer: bool,
}

/// Les zones qui jouent ce disque ou le tiennent en pause, triées.
async fn zones_qui_jouent_le_disque(etat: &EtatRoutes) -> Vec<i64> {
    let candidates: Vec<i64> = etat.zones.lock().await.iter().copied().collect();
    let mut v = Vec::new();
    for z in candidates {
        if etat.hote.source_en_cours(z).await.as_deref() == Some(SOURCE) {
            v.push(z);
        }
    }
    v.sort_unstable();
    v
}

/// Fil 2135 — éjecter le disque. Pas d'extraction à protéger : ce serveur
/// n'en lance aucune (`/cd-rip`, #2466) ; seule la LECTURE tient le disque.
async fn ejecter(State(etat): State<EtatRoutes>, corps: Option<Json<DemandeEjecter>>) -> Response {
    let forcer = corps.map(|Json(d)| d.forcer).unwrap_or(false);
    let Some(lecteur) = etat.lecteur.clone() else {
        let (code, motif, message) = aucun_lecteur();
        return refus(code, motif, message);
    };
    let l = lecteur.clone();
    let presence = tokio::task::spawn_blocking(move || l.presence())
        .await
        .unwrap_or(Presence::AucunLecteur);
    match presence {
        Presence::AucunLecteur => {
            let (code, motif, message) = aucun_lecteur();
            return refus(code, motif, message);
        }
        Presence::Vide => {
            return refus(
                StatusCode::CONFLICT,
                "aucun_disque",
                "Le lecteur est vide.".into(),
            );
        }
        Presence::Disque => {}
    }

    let zones = zones_qui_jouent_le_disque(&etat).await;
    if !zones.is_empty() && !forcer {
        return (
            StatusCode::CONFLICT,
            Json(json!({
                "error": "lecture_en_cours",
                "message": format!(
                    "Le disque est en lecture sur {} zone(s) : arrêtez la lecture, \
                     ou confirmez l'éjection pour l'arrêter.",
                    zones.len()
                ),
                "zones": zones,
            })),
        )
            .into_response();
    }
    // La lecture d'abord : une zone arrêtée ne lit plus de secteurs, et le
    // système ne refuse pas l'éjection d'un disque qu'on lit encore.
    for &z in &zones {
        tracing::info!(zone_id = z, "cd_zone_arretee_avant_ejection");
        etat.hote.arreter(z).await;
    }
    {
        let mut suivies = etat.zones.lock().await;
        for z in &zones {
            suivies.remove(z);
        }
    }

    let l = lecteur.clone();
    let resultat = tokio::task::spawn_blocking(move || l.ejecter_disque())
        .await
        .unwrap_or_else(|e| Err(ErreurEjection::Echec(e.to_string())));
    // Dans tous les cas : la présence a pu changer (éjection partielle).
    etat.reveil.notify_one();
    match resultat {
        Ok(()) => Json(json!({
            "ejecte": true,
            "lecteur": lecteur.chemin(),
            "zones_arretees": zones,
        }))
        .into_response(),
        Err(ErreurEjection::AucunLecteur) => {
            let (code, motif, message) = aucun_lecteur();
            refus(code, motif, message)
        }
        Err(ErreurEjection::AucunDisque) => refus(
            StatusCode::CONFLICT,
            "aucun_disque",
            "Le lecteur est vide.".into(),
        ),
        Err(ErreurEjection::NonPrisEnCharge) => refus(
            StatusCode::NOT_IMPLEMENTED,
            "ejection_non_prise_en_charge",
            "Ce lecteur ne peut pas être éjecté par Tune.".into(),
        ),
        Err(ErreurEjection::Echec(raison)) => refus(
            StatusCode::BAD_GATEWAY,
            "ejection",
            format!("Le système a refusé l'éjection : {raison}"),
        ),
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
                reveil: Arc::default(),
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

        // #5161 — un lecteur à chaud pas encore branché : « aucun lecteur »,
        // pas « lecteur vide ».
        let l = LecteurSimule::new(toc_du_vecteur());
        l.debrancher();
        let (e, _) = etat(Some(Arc::new(l)), Arc::new(SansReseau));
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

    // ─── Fil 2135 : éjecter ────────────────────────────────────────────────

    fn simule_arc() -> Arc<LecteurSimule> {
        Arc::new(LecteurSimule::new(toc_du_vecteur()))
    }

    /// La surveillance a-t-elle été réveillée ? (`Notify` garde le jeton.)
    async fn reveillee(e: &EtatRoutes) -> bool {
        tokio::time::timeout(std::time::Duration::from_millis(50), e.reveil.notified())
            .await
            .is_ok()
    }

    #[tokio::test]
    async fn ejecter_sans_lecture_ejecte_le_disque_et_reveille_la_surveillance() {
        let l = simule_arc();
        let (e, hote) = etat(Some(l.clone()), Arc::new(SansReseau));
        let (code, v) = appel(router(e.clone()), "POST", "/ejecter", None).await;
        assert_eq!(code, StatusCode::OK, "{v}");
        assert_eq!(
            v,
            json!({"ejecte": true, "lecteur": "simulé", "zones_arretees": []})
        );
        assert_eq!(l.ejections(), 1);
        assert_eq!(l.presence(), Presence::Vide);
        assert!(hote.arrets.lock().await.is_empty());
        assert!(
            reveillee(&e).await,
            "sources.changed doit partir tout de suite"
        );
        // Plus rien à éjecter.
        let (code, v) = appel(router(e), "POST", "/ejecter", None).await;
        assert_eq!(code, StatusCode::CONFLICT);
        assert_eq!(v["error"], "aucun_disque");
        assert_eq!(l.ejections(), 1);
    }

    /// Une zone joue le disque : sans confirmation, REFUS qui nomme la zone,
    /// et rien n'est touché — ni la zone, ni le disque.
    #[tokio::test]
    async fn ejecter_pendant_la_lecture_est_refuse_sans_confirmation() {
        let l = simule_arc();
        let (e, hote) = etat(Some(l.clone()), Arc::new(SansReseau));
        let (code, _) = appel(
            router(e.clone()),
            "POST",
            "/jouer",
            Some(json!({"zone_id": 3})),
        )
        .await;
        assert_eq!(code, StatusCode::OK);
        let (code, v) = appel(router(e.clone()), "POST", "/ejecter", None).await;
        assert_eq!(code, StatusCode::CONFLICT, "{v}");
        assert_eq!(v["error"], "lecture_en_cours");
        assert_eq!(v["zones"], json!([3]));
        assert!(v["message"].as_str().unwrap().contains("1 zone"));
        assert_eq!(l.ejections(), 0, "le disque n'est pas éjecté");
        assert_eq!(l.presence(), Presence::Disque);
        assert!(hote.arrets.lock().await.is_empty(), "la zone joue encore");
        assert!(e.zones.lock().await.contains(&3));
        // `forcer: false` explicite : même refus.
        let (code, _) = appel(
            router(e),
            "POST",
            "/ejecter",
            Some(json!({"forcer": false})),
        )
        .await;
        assert_eq!(code, StatusCode::CONFLICT);
        assert_eq!(l.ejections(), 0);
    }

    /// Confirmé (`forcer`) : la zone est arrêtée D'ABORD, puis le disque est
    /// éjecté. Une zone passée à autre chose (radio) n'est pas touchée.
    #[tokio::test]
    async fn ejecter_confirme_arrete_la_lecture_puis_ejecte() {
        let l = simule_arc();
        let (e, hote) = etat(Some(l.clone()), Arc::new(SansReseau));
        for z in [3, 5] {
            let (code, _) = appel(
                router(e.clone()),
                "POST",
                "/jouer",
                Some(json!({"zone_id": z})),
            )
            .await;
            assert_eq!(code, StatusCode::OK);
        }
        hote.sources.lock().await.insert(5, "radio".into());
        let (code, v) = appel(
            router(e.clone()),
            "POST",
            "/ejecter",
            Some(json!({"forcer": true})),
        )
        .await;
        assert_eq!(code, StatusCode::OK, "{v}");
        assert_eq!(v["zones_arretees"], json!([3]));
        assert_eq!(*hote.arrets.lock().await, vec![3]);
        assert_eq!(l.ejections(), 1);
        assert!(!e.zones.lock().await.contains(&3));
    }

    #[tokio::test]
    async fn les_refus_d_ejection_disent_pourquoi() {
        let (e, _) = etat(None, Arc::new(SansReseau));
        let (code, v) = appel(router(e), "POST", "/ejecter", None).await;
        assert_eq!(
            (code, v["error"].as_str()),
            (StatusCode::NOT_FOUND, Some("aucun_lecteur"))
        );

        let l = simule_arc();
        l.debrancher();
        let (e, _) = etat(Some(l), Arc::new(SansReseau));
        let (code, v) = appel(router(e), "POST", "/ejecter", None).await;
        assert_eq!(
            (code, v["error"].as_str()),
            (StatusCode::NOT_FOUND, Some("aucun_lecteur"))
        );

        // Le système refuse (disque occupé) : 502, la raison du système.
        let l = simule_arc();
        l.refuser_ejection("Device or resource busy");
        let (e, _) = etat(Some(l.clone()), Arc::new(SansReseau));
        let (code, v) = appel(router(e), "POST", "/ejecter", None).await;
        assert_eq!(code, StatusCode::BAD_GATEWAY);
        assert_eq!(v["error"], "ejection");
        assert!(
            v["message"]
                .as_str()
                .unwrap()
                .contains("Device or resource busy")
        );
        assert_eq!(l.presence(), Presence::Disque);

        // Un lecteur qui ne sait pas éjecter (volume fabriqué) : 501.
        let v =
            crate::cddafs::tests::faux_volume("ejecter-501", &crate::cddafs::tests::petite_toc());
        let l = crate::cddafs::LecteurVolume::sur_dossier(v.to_path_buf());
        let (e, _) = etat(Some(Arc::new(l)), Arc::new(SansReseau));
        let (code, v) = appel(router(e), "POST", "/ejecter", None).await;
        assert_eq!(code, StatusCode::NOT_IMPLEMENTED);
        assert_eq!(v["error"], "ejection_non_prise_en_charge");
    }
}
