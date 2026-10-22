//! Les routes du greffon, montées par l'hôte sous `/api/v1/ext/cd`.
//!
//! * `GET  /etat`   — le lecteur et le disque ;
//! * `GET  /disque` — la TOC, l'identifiant et les métadonnées ;
//! * `POST /jouer`  — `{ "zone_id": 3, "piste": 5 }` : pose le disque entier
//!   en file sur la zone et joue la piste demandée (la 1ʳᵉ sans `piste`) ;
//! * `POST /ejecter` — `{ "forcer": true }` facultatif : éjecte le disque du
//!   lecteur courant (fil 2135). Si une zone le joue (ou le tient en pause),
//!   refus `409 lecture_en_cours` qui nomme les zones ; avec `forcer`, ces
//!   zones sont d'abord arrêtées, puis le disque est éjecté. #6410 : si le
//!   disque est ENTIER en mémoire, ni refus ni arrêt — la lecture continue
//!   depuis la mémoire (`lecture_continue` nomme les zones).
//!
//! * `GET  /memoire` — « Charger le CD en mémoire » (#6043) : réglages, RAM
//!   disponible et progression du chargement ;
//! * `POST /memoire` — `{ "actif": true, "plafond_mio": 800 }`, chacun
//!   facultatif ; désactiver libère le disque chargé.
//! * `POST /memoire/charger` — #6596 : charge le disque inséré en mémoire SANS
//!   rien jouer (ni file ni lecture d'aucune zone touchée) et rend l'état de
//!   `GET /memoire`. La progression part ensuite en évènements
//!   `cd.memoire.progression` puis `cd.memoire.termine` ; `POST /jouer` lancé
//!   après se sert dans la mémoire. Refus : `404 memoire_indisponible`,
//!   `409 memoire_desactivee` (réglage coupé), `409 extraction_en_cours`,
//!   `409 memoire_occupee` (un AUTRE disque tient la mémoire et une zone le
//!   joue, ou son chargement est en cours : le charger le couperait), et
//!   ceux de `/disque` (`aucun_lecteur`, `aucun_disque`, `lecture_toc`).
//!
//! `GET /etat` porte aussi `chargement` : la progression du disque chargé en
//! mémoire, `null` sans chargement.
//!
//! Pendant une extraction (#2466, `extraction/routes.rs`), `/jouer` et
//! `/ejecter` refusent par `409 extraction_en_cours`, même avec `forcer` :
//! on annule d'abord l'extraction.

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::Notify;
use tune_core::db::backend::DbBackend;
use tune_core::db::settings_repo::SettingsRepo;

use crate::discid::disc_id;
use crate::ejection::ZonesDuDisque;
use crate::extraction::Extractions;
use crate::fournisseur::{SOURCE, source_id};
use crate::hote::{ElementFile, HoteLecture};
use crate::lecteur::{
    ErreurCd, ErreurEjection, LecteurDisque, Presence, plateforme_prise_en_charge,
};
use crate::memoire::{CLE_ACTIF, CLE_PLAFOND_MIO, MIO, MemoireCd};
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
    /// #2466 — les extractions. `None` : routes d'extraction absentes.
    pub extraction: Option<Arc<Extractions>>,
    /// #6043 — le disque chargé en mémoire. `None` : pas de chargement.
    pub memoire: Option<Arc<MemoireCd>>,
    /// #6043 — pour ranger les réglages du chargement. `None` : non rangés.
    pub backend: Option<Arc<dyn DbBackend>>,
}

pub fn router(etat: EtatRoutes) -> Router<()> {
    let base = Router::new()
        .route("/etat", get(etat_du_lecteur))
        .route("/disque", get(disque))
        .route("/jouer", post(jouer))
        .route("/ejecter", post(ejecter))
        .route("/memoire", get(memoire).post(regler_memoire))
        .route("/memoire/charger", post(charger_en_memoire))
        .with_state(etat.clone());
    match etat.extraction.clone() {
        Some(ex) => base.merge(crate::extraction::routes::router(etat, ex)),
        None => base,
    }
}

/// L'extraction en cours, s'il y en a une (#2466).
fn extraction_en_cours(etat: &EtatRoutes) -> Option<String> {
    etat.extraction
        .as_ref()
        .and_then(|e| e.en_cours())
        .map(|t| t.id.clone())
}

pub(crate) fn refus(code: StatusCode, motif: &str, message: String) -> Response {
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
pub(crate) async fn toc_ou_refus(etat: &EtatRoutes) -> Result<Toc, Refus> {
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
        "chargement": etat.memoire.as_ref().and_then(|m| m.progression()),
    }))
}

/// #6043 — les réglages du chargement en mémoire et sa progression.
fn etat_memoire(m: Option<&MemoireCd>) -> Value {
    match m {
        Some(m) => {
            let r = m.reglages();
            json!({
                "disponible": true,
                "actif": r.actif,
                "plafond_mio": r.plafond_octets / MIO,
                "ram_disponible_mio": m.ram_disponible().map(|o| o / MIO),
                "chargement": m.progression(),
            })
        }
        None => json!({ "disponible": false, "actif": false, "chargement": null }),
    }
}

async fn memoire(State(etat): State<EtatRoutes>) -> Json<Value> {
    Json(etat_memoire(etat.memoire.as_deref()))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DemandeMemoire {
    actif: Option<bool>,
    plafond_mio: Option<u64>,
}

async fn regler_memoire(State(etat): State<EtatRoutes>, Json(d): Json<DemandeMemoire>) -> Response {
    let Some(m) = etat.memoire.clone() else {
        return refus(
            StatusCode::NOT_FOUND,
            "memoire_indisponible",
            "Le chargement en mémoire n'est pas disponible.".into(),
        );
    };
    if let Some(backend) = etat.backend.clone() {
        let repo = SettingsRepo::with_backend(backend);
        let mut r = Ok(());
        if let Some(a) = d.actif {
            r = r.and(repo.set(CLE_ACTIF, if a { "true" } else { "false" }));
        }
        if let Some(p) = d.plafond_mio {
            r = r.and(repo.set(CLE_PLAFOND_MIO, &p.to_string()));
        }
        if let Err(e) = r {
            return refus(StatusCode::INTERNAL_SERVER_ERROR, "reglage", e);
        }
    }
    let (actif, plafond) = (d.actif, d.plafond_mio.map(|p| p * MIO));
    let m2 = m.clone();
    let _ = tokio::task::spawn_blocking(move || m2.regler(actif, plafond)).await;
    Json(etat_memoire(Some(&m))).into_response()
}

/// #6596 — « Charger en mémoire » : le disque inséré, sans rien jouer. Ce
/// geste ne passe JAMAIS par `etat.hote` : la file et la lecture des zones
/// restent ce qu'elles sont, rien n'est coupé d'office.
async fn charger_en_memoire(State(etat): State<EtatRoutes>) -> Response {
    let Some(m) = etat.memoire.clone() else {
        return refus(
            StatusCode::NOT_FOUND,
            "memoire_indisponible",
            "Le chargement en mémoire n'est pas disponible.".into(),
        );
    };
    if !m.reglages().actif {
        return refus(
            StatusCode::CONFLICT,
            "memoire_desactivee",
            "Le chargement du CD en mémoire est désactivé dans les réglages.".into(),
        );
    }
    if extraction_en_cours(&etat).is_some() {
        return refus(
            StatusCode::CONFLICT,
            "extraction_en_cours",
            "Le disque est en cours d'extraction : annulez-la pour le charger.".into(),
        );
    }
    let Some(lecteur) = etat.lecteur.clone() else {
        let (code, motif, message) = aucun_lecteur();
        return refus(code, motif, message);
    };
    // La génération AVANT la TOC, comme `FournisseurCd::ouvrir` : un lecteur
    // changé entre les deux fait tomber le chargement en `ejecte`.
    let l = lecteur.clone();
    let generation = tokio::task::spawn_blocking(move || l.generation_lecteur())
        .await
        .unwrap_or_default();
    let toc = match toc_ou_refus(&etat).await {
        Ok(v) => v,
        Err((code, motif, message)) => return refus(code, motif, message),
    };
    let disc = disc_id(&toc);
    // Relecture rc7 — un AUTRE disque tient la mémoire (A gardé après son
    // éjection, #6410, ou encore en chargement). Le libérer couperait la
    // zone qui le joue : on refuse, rien n'est coupé d'office. Gardé mais
    // joué par personne, il ne sert plus : le chargement de B le remplace.
    let m2 = m.clone();
    let d2 = disc.clone();
    let autre = tokio::task::spawn_blocking(move || m2.autre_disque_en_memoire(&d2))
        .await
        .ok()
        .flatten();
    if let Some((autre, en_chargement)) = autre {
        let zones = zones_qui_jouent_le_disque(&etat).await;
        if en_chargement || !zones.is_empty() {
            tracing::info!(disc_id = %disc, en_memoire = %autre, zones = ?zones, "cd_memoire_occupee_refus");
            return (
                StatusCode::CONFLICT,
                Json(json!({
                    "error": "memoire_occupee",
                    "message": "Un autre disque est encore en mémoire et joue : \
                                attendez la fin de sa lecture ou arrêtez-la avant de charger celui-ci.",
                    "disc_id_en_memoire": autre,
                    "zones": zones,
                })),
            )
                .into_response();
        }
    }
    let m2 = m.clone();
    let lance = tokio::task::spawn_blocking(move || {
        m2.charger_le_disque(&lecteur, &toc, &disc, generation)
            .is_some()
    })
    .await
    .unwrap_or(false);
    if !lance {
        return refus(
            StatusCode::CONFLICT,
            "memoire_desactivee",
            "Le chargement du CD en mémoire est désactivé dans les réglages.".into(),
        );
    }
    Json(etat_memoire(Some(&m))).into_response()
}

/// #6043 — rend la mémoire du disque chargé (éjection, extraction).
pub(crate) async fn liberer_memoire(etat: &EtatRoutes) {
    if let Some(m) = etat.memoire.clone() {
        let _ = tokio::task::spawn_blocking(move || m.liberer()).await;
    }
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
    if extraction_en_cours(etat).is_some() {
        return Err((
            StatusCode::CONFLICT,
            "extraction_en_cours",
            "Le disque est en cours d'extraction : annulez-la pour le jouer.".into(),
        ));
    }
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
pub(crate) async fn zones_qui_jouent_le_disque(etat: &EtatRoutes) -> Vec<i64> {
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

/// Fil 2135 — éjecter le disque. La lecture et l'extraction (#2466) tiennent
/// le disque : la première s'arrête sur confirmation, la seconde s'annule.
async fn ejecter(State(etat): State<EtatRoutes>, corps: Option<Json<DemandeEjecter>>) -> Response {
    let forcer = corps.map(|Json(d)| d.forcer).unwrap_or(false);
    if let Some(id) = extraction_en_cours(&etat) {
        return crate::extraction::routes::conflit_extraction(&id);
    }
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

    // #6410 — le disque est ENTIER en mémoire : comme sur Daphile, la
    // lecture continue après l'éjection, sans confirmation ni arrêt. Le
    // surveillant garde la mémoire au passage à « vide ».
    let m = etat.memoire.clone();
    let en_memoire =
        tokio::task::spawn_blocking(move || m.is_some_and(|m| m.disque_complet().is_some()))
            .await
            .unwrap_or(false);
    if en_memoire {
        let continuent = zones_qui_jouent_le_disque(&etat).await;
        tracing::info!(zones = ?continuent, "cd_ejection_disque_entier_en_memoire");
        let l = lecteur.clone();
        let resultat = tokio::task::spawn_blocking(move || l.ejecter_disque())
            .await
            .unwrap_or_else(|e| Err(ErreurEjection::Echec(e.to_string())));
        etat.reveil.notify_one();
        return match resultat {
            Ok(()) => Json(json!({
                "ejecte": true,
                "lecteur": lecteur.chemin(),
                "zones_arretees": [],
                "lecture_continue": continuent,
            }))
            .into_response(),
            Err(e) => refus_d_ejection(e),
        };
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

    // #6043 — le chargement en mémoire lit encore le disque : l'arrêter
    // avant d'éjecter, et rendre la mémoire.
    liberer_memoire(&etat).await;
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
        Err(e) => refus_d_ejection(e),
    }
}

fn refus_d_ejection(e: ErreurEjection) -> Response {
    match e {
        ErreurEjection::AucunLecteur => {
            let (code, motif, message) = aucun_lecteur();
            refus(code, motif, message)
        }
        ErreurEjection::AucunDisque => refus(
            StatusCode::CONFLICT,
            "aucun_disque",
            "Le lecteur est vide.".into(),
        ),
        ErreurEjection::NonPrisEnCharge => refus(
            StatusCode::NOT_IMPLEMENTED,
            "ejection_non_prise_en_charge",
            "Ce lecteur ne peut pas être éjecté par Tune.".into(),
        ),
        ErreurEjection::Echec(raison) => refus(
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
                memoire: None,
                backend: None,
                extraction: None,
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

    /// #6043 — `/memoire` lit et règle le chargement ; `/etat` en porte la
    /// progression.
    #[tokio::test]
    async fn memoire_se_regle_et_etat_porte_la_progression() {
        use crate::memoire::{Fin, MemoireCd, Reglages};
        use crate::toc::{PisteToc, Toc};
        let toc = Toc::nouvelle(
            vec![PisteToc {
                numero: 1,
                debut: 0,
                audio: true,
            }],
            100,
        )
        .unwrap();
        let l: Arc<dyn LecteurDisque> = Arc::new(LecteurSimule::new(toc.clone()));
        let m = Arc::new(MemoireCd::new(Reglages {
            actif: true,
            plafond_octets: u64::MAX,
        }));
        let (mut e, _) = etat(Some(l.clone()), Arc::new(SansReseau));
        e.memoire = Some(m.clone());

        let (_, v) = appel(router(e.clone()), "GET", "/etat", None).await;
        assert!(v["chargement"].is_null(), "{v}");
        let c = m.chargement(&l, &toc, &disc_id(&toc), 0, 0).unwrap();
        assert_eq!(c.attendre_la_fin(), Some(Fin::Termine));
        let (_, v) = appel(router(e.clone()), "GET", "/etat", None).await;
        assert_eq!(v["chargement"]["pourcentage"], 100, "{v}");
        assert_eq!(v["chargement"]["fin"], "termine");
        assert_eq!(v["chargement"]["secteurs_total"], 100);

        let (code, v) = appel(router(e.clone()), "GET", "/memoire", None).await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(
            (v["disponible"].clone(), v["actif"].clone()),
            (json!(true), json!(true))
        );
        let (code, v) = appel(
            router(e.clone()),
            "POST",
            "/memoire",
            Some(json!({ "actif": false })),
        )
        .await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(v["actif"], false);
        assert!(v["chargement"].is_null(), "désactiver libère : {v}");
        assert!(!m.reglages().actif);

        // Sans chargement en mémoire (greffon ancien) : 404 au réglage.
        let (e, _) = etat(simule(), Arc::new(SansReseau));
        let (code, _) = appel(
            router(e),
            "POST",
            "/memoire",
            Some(json!({ "actif": true })),
        )
        .await;
        assert_eq!(code, StatusCode::NOT_FOUND);
    }

    // ─── #6596 : charger en mémoire sans jouer ────────────────────────────

    /// Trois pistes : 300, 800 et 400 secteurs.
    fn petite_toc() -> Toc {
        use crate::toc::PisteToc;
        let p = |numero, debut| PisteToc {
            numero,
            debut,
            audio: true,
        };
        Toc::nouvelle(vec![p(1, 0), p(2, 300), p(3, 1_100)], 1_500).unwrap()
    }

    /// Un lecteur simulé, la mémoire branchée sur un VRAI bus (l'observateur
    /// de production), et la zone 4 qui joue la radio.
    async fn banc_memoire(
        l: Arc<LecteurSimule>,
        actif: bool,
    ) -> (
        EtatRoutes,
        Arc<HoteTemoin>,
        Arc<MemoireCd>,
        tokio::sync::broadcast::Receiver<tune_core::event_bus::TuneEvent>,
    ) {
        use crate::memoire::{Reglages, observateur_du_bus};
        let bus = tune_core::event_bus::EventBus::new();
        let rx = bus.subscribe();
        let m = Arc::new(MemoireCd::new(Reglages {
            actif,
            plafond_octets: u64::MAX,
        }));
        m.observer(observateur_du_bus(bus));
        let (mut e, hote) = etat(Some(l), Arc::new(SansReseau));
        e.memoire = Some(m.clone());
        hote.sources.lock().await.insert(4, "radio".into());
        (e, hote, m, rx)
    }

    /// Les évènements `cd.memoire.*` reçus, jusqu'à `cd.memoire.termine`
    /// (2 s au plus).
    async fn evenements_du_chargement(
        rx: &mut tokio::sync::broadcast::Receiver<tune_core::event_bus::TuneEvent>,
    ) -> Vec<(String, Value)> {
        use crate::memoire::EVT_TERMINE;
        let mut v = Vec::new();
        let lire = async {
            loop {
                let ev = rx.recv().await.unwrap();
                if !ev.event_type.starts_with("cd.memoire.") {
                    continue;
                }
                let fin = ev.event_type == EVT_TERMINE;
                v.push((ev.event_type, ev.data));
                if fin {
                    break;
                }
            }
        };
        tokio::time::timeout(std::time::Duration::from_secs(2), lire)
            .await
            .expect("cd.memoire.termine n'est jamais parti");
        v
    }

    /// Ce que la zone 4 joue, et ce qu'on a demandé à l'hôte : rien ne doit
    /// avoir bougé.
    async fn rien_n_a_bouge(e: &EtatRoutes, hote: &HoteTemoin) {
        let files = hote.files.lock().await.len();
        assert_eq!(files, 0, "charger en mémoire a posé une file");
        assert!(
            hote.arrets.lock().await.is_empty(),
            "charger en mémoire a arrêté une zone"
        );
        assert_eq!(
            hote.sources.lock().await.get(&4).map(String::as_str),
            Some("radio"),
            "la zone 4 ne joue plus sa radio"
        );
        assert!(e.zones.lock().await.is_empty(), "une zone suit le disque");
    }

    /// #6596 — `POST /memoire/charger` charge le disque ENTIER sans toucher
    /// à la file ni à la lecture d'aucune zone ; la progression part en
    /// évènements jusqu'à « prêt », et `GET /memoire` la rend. Un second
    /// appel reprend le même chargement, sans relire le lecteur.
    #[tokio::test]
    async fn charger_en_memoire_ne_touche_ni_a_la_file_ni_a_la_lecture() {
        use crate::memoire::{EVT_PROGRESSION, EVT_TERMINE, Fin};
        let toc = petite_toc();
        let l = Arc::new(LecteurSimule::new(toc.clone()));
        let (e, hote, m, mut rx) = banc_memoire(l.clone(), true).await;

        let (code, v) = appel(router(e.clone()), "POST", "/memoire/charger", None).await;
        assert_eq!(code, StatusCode::OK, "{v}");
        assert_eq!(v["chargement"]["disc_id"], disc_id(&toc), "{v}");
        let evs = evenements_du_chargement(&mut rx).await;
        let c = m.courant().unwrap();
        assert_eq!(c.attendre_la_fin(), Some(Fin::Termine));

        // Les états : en cours (pourcentages croissants, sans fin), puis prêt.
        let (dernier, avant) = evs.split_last().unwrap();
        assert!(!avant.is_empty(), "aucune progression annoncée : {evs:?}");
        assert!(
            avant
                .iter()
                .any(|(_, d)| d["pourcentage"].as_i64().is_some_and(|p| p > 0)),
            "aucune progression intermédiaire annoncée : {evs:?}"
        );
        let mut pct = -1i64;
        for (t, d) in avant {
            assert_eq!(t, EVT_PROGRESSION, "{evs:?}");
            assert!(d["fin"].is_null(), "{d}");
            let p = d["pourcentage"].as_i64().unwrap();
            assert!(p > pct && p < 100, "progression {p} après {pct}");
            pct = p;
        }
        assert_eq!(dernier.0, EVT_TERMINE);
        assert_eq!(dernier.1["fin"], "termine");
        assert_eq!(dernier.1["pourcentage"], 100);
        assert_eq!(dernier.1["disc_id"], disc_id(&toc));

        let (_, v) = appel(router(e.clone()), "GET", "/memoire", None).await;
        assert_eq!(v["chargement"]["fin"], "termine", "{v}");
        assert_eq!(v["chargement"]["pistes_chargees"], json!([1, 2, 3]));
        rien_n_a_bouge(&e, &hote).await;

        let appels = l.appels();
        let (code, _) = appel(router(e.clone()), "POST", "/memoire/charger", None).await;
        assert_eq!(code, StatusCode::OK);
        assert!(Arc::ptr_eq(&m.courant().unwrap(), &c), "rechargé de zéro");
        assert_eq!(l.appels(), appels, "le disque chargé a été relu");
        rien_n_a_bouge(&e, &hote).await;
        m.liberer();
    }

    /// #6596 — la lecture lancée APRÈS le chargement se sert dans la copie
    /// en mémoire : les octets de la piste sont exacts et le lecteur n'est
    /// plus lu. « Lire le disque » reste le geste qui remplace la file.
    #[tokio::test]
    async fn lire_apres_le_chargement_se_sert_dans_la_memoire() {
        use crate::fournisseur::FournisseurCd;
        use crate::memoire::Fin;
        use crate::simule::contenu_des_secteurs;
        use std::io::Read;
        use tune_core::source_pcm::FournisseurPcm;
        let l = Arc::new(LecteurSimule::new(petite_toc()));
        let (e, hote, m, mut rx) = banc_memoire(l.clone(), true).await;
        let (code, _) = appel(router(e.clone()), "POST", "/memoire/charger", None).await;
        assert_eq!(code, StatusCode::OK);
        evenements_du_chargement(&mut rx).await;
        let c = m.courant().unwrap();
        assert_eq!(c.attendre_la_fin(), Some(Fin::Termine));
        let appels = l.appels();

        let (code, v) = appel(
            router(e.clone()),
            "POST",
            "/jouer",
            Some(json!({"zone_id": 4, "piste": 2})),
        )
        .await;
        assert_eq!(code, StatusCode::OK, "{v}");
        let source_id = {
            let files = hote.files.lock().await;
            assert_eq!((files[0].0, files[0].2), (4, 1));
            files[0].1[1].source_id.clone()
        };
        // Le chemin de l'hôte : l'orchestrateur ouvre la piste par la source
        // PCM `cd`, c'est-à-dire ce fournisseur.
        let f = FournisseurCd {
            lecteur: l.clone(),
            memoire: Some(m.clone()),
        };
        let mut flux = f.ouvrir(&source_id, 0).unwrap();
        let mut v = Vec::new();
        flux.lecteur.read_to_end(&mut v).unwrap();
        assert_eq!(v, contenu_des_secteurs(300, 800), "piste 2, au bit près");
        assert_eq!(
            l.appels(),
            appels,
            "la lecture a relu le lecteur : elle ne sert pas la copie en mémoire"
        );
        assert!(
            Arc::ptr_eq(&m.courant().unwrap(), &c),
            "la lecture a relancé un autre chargement"
        );
        m.liberer();
    }

    /// #6596, relecture rc7 — A est entier en mémoire, éjecté (gardé, #6410)
    /// et joué par la zone 7 ; B est inséré. « Charger en mémoire » B est
    /// REFUSÉ (`409 memoire_occupee`) : A reste en mémoire et lisible, sa
    /// zone n'est pas touchée. Plus personne ne joue A : B se charge.
    #[tokio::test]
    async fn charger_un_autre_disque_ne_coupe_pas_celui_qui_joue_depuis_la_memoire() {
        use crate::fournisseur::{FournisseurCd, source_id};
        use crate::memoire::Fin;
        use crate::simule::contenu_des_secteurs;
        use std::io::Read;
        use tune_core::source_pcm::FournisseurPcm;
        let toc = petite_toc();
        let l = Arc::new(LecteurSimule::new(toc.clone()));
        let (e, hote, m, _rx) = banc_memoire(l.clone(), true).await;
        // A : entier en mémoire, joué par la zone 7, puis éjecté et gardé.
        let ld: Arc<dyn LecteurDisque> = l.clone();
        let a = m.chargement(&ld, &toc, "disque-A", 0, 0).unwrap();
        assert_eq!(a.attendre_la_fin(), Some(Fin::Termine));
        hote.sources.lock().await.insert(7, SOURCE.into());
        e.zones.lock().await.insert(7);
        l.ejecter();
        assert!(m.garder_apres_ejection(), "A gardé après son éjection");
        // B inséré (un autre disc_id que A).
        l.inserer();
        assert_ne!(disc_id(&toc), "disque-A");

        let (code, v) = appel(router(e.clone()), "POST", "/memoire/charger", None).await;
        assert_eq!(code, StatusCode::CONFLICT, "{v}");
        assert_eq!(v["error"], "memoire_occupee");
        assert_eq!(v["disc_id_en_memoire"], "disque-A");
        assert_eq!(v["zones"], json!([7]));

        assert!(
            m.disque_garde().is_some_and(|g| Arc::ptr_eq(&g, &a)),
            "A a été libéré : la zone qui le jouait est coupée"
        );
        let f = FournisseurCd {
            lecteur: l.clone(),
            memoire: Some(m.clone()),
        };
        let mut flux = f.ouvrir(&source_id("disque-A", 2), 0).unwrap();
        let mut lu = Vec::new();
        flux.lecteur.read_to_end(&mut lu).unwrap();
        assert_eq!(lu, contenu_des_secteurs(300, 800), "A n'est plus lisible");
        assert!(
            hote.arrets.lock().await.is_empty(),
            "une zone a été arrêtée"
        );
        assert!(hote.files.lock().await.is_empty(), "une file a été posée");

        // La zone 7 passe à autre chose : plus rien ne joue A, B se charge.
        hote.sources.lock().await.insert(7, "radio".into());
        let (code, v) = appel(router(e.clone()), "POST", "/memoire/charger", None).await;
        assert_eq!(code, StatusCode::OK, "{v}");
        assert_eq!(v["chargement"]["disc_id"], disc_id(&toc));
        m.liberer();
    }

    /// #6596 — les refus disent pourquoi, et un chargement qui échoue (disque
    /// retiré en route) l'annonce : `fin` = `ejecte`, mémoire rendue.
    #[tokio::test]
    async fn charger_en_memoire_refuse_ou_echoue_en_le_disant() {
        // Sans chargement en mémoire (greffon ancien) : 404.
        let (e, _) = etat(simule(), Arc::new(SansReseau));
        let (code, v) = appel(router(e), "POST", "/memoire/charger", None).await;
        assert_eq!(code, StatusCode::NOT_FOUND);
        assert_eq!(v["error"], "memoire_indisponible");

        // Réglage coupé : 409, rien n'est lancé.
        let l = Arc::new(LecteurSimule::new(petite_toc()));
        let (e, hote, m, _rx) = banc_memoire(l.clone(), false).await;
        let (code, v) = appel(router(e.clone()), "POST", "/memoire/charger", None).await;
        assert_eq!(code, StatusCode::CONFLICT);
        assert_eq!(v["error"], "memoire_desactivee");
        assert!(m.courant().is_none());
        assert_eq!(l.appels(), 0);
        rien_n_a_bouge(&e, &hote).await;

        // Lecteur vide : 409 aucun_disque.
        let l = Arc::new(LecteurSimule::new(petite_toc()));
        l.ejecter();
        let (e, _, m, _rx) = banc_memoire(l, true).await;
        let (code, v) = appel(router(e), "POST", "/memoire/charger", None).await;
        assert_eq!(code, StatusCode::CONFLICT);
        assert_eq!(v["error"], "aucun_disque");
        assert!(m.courant().is_none());

        // Retiré pendant le chargement : l'échec est annoncé.
        let l = Arc::new(LecteurSimule::new(petite_toc()));
        l.ejecter_apres(3);
        let (e, hote, m, mut rx) = banc_memoire(l, true).await;
        let (code, _) = appel(router(e.clone()), "POST", "/memoire/charger", None).await;
        assert_eq!(code, StatusCode::OK);
        let evs = evenements_du_chargement(&mut rx).await;
        let (t, d) = evs.last().unwrap();
        assert_eq!(t, crate::memoire::EVT_TERMINE);
        assert_eq!(d["fin"], "ejecte", "{d}");
        assert_eq!(
            (d["octets_ram"].clone(), d["octets_fichier"].clone()),
            (json!(0), json!(0))
        );
        let (_, v) = appel(router(e.clone()), "GET", "/memoire", None).await;
        assert_eq!(v["chargement"]["fin"], "ejecte", "{v}");
        rien_n_a_bouge(&e, &hote).await;
        m.liberer();
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

    /// #6410 — le disque est ENTIER en mémoire : l'éjection passe sans
    /// confirmation, n'arrête aucune zone et ne rend pas la mémoire.
    #[tokio::test]
    async fn ejecter_un_disque_entier_en_memoire_ne_coupe_pas_la_lecture() {
        use crate::memoire::{Fin, Reglages};
        use crate::toc::PisteToc;
        let p = |numero, debut| PisteToc {
            numero,
            debut,
            audio: true,
        };
        let toc = Toc::nouvelle(vec![p(1, 0), p(2, 300)], 600).unwrap();
        let l = Arc::new(LecteurSimule::new(toc.clone()));
        let (mut e, hote) = etat(Some(l.clone()), Arc::new(SansReseau));
        let m = Arc::new(MemoireCd::new(Reglages {
            actif: true,
            plafond_octets: u64::MAX,
        }));
        e.memoire = Some(m.clone());
        let (code, _) = appel(
            router(e.clone()),
            "POST",
            "/jouer",
            Some(json!({"zone_id": 3})),
        )
        .await;
        assert_eq!(code, StatusCode::OK);
        let ld: Arc<dyn LecteurDisque> = l.clone();
        let c = m.chargement(&ld, &toc, &disc_id(&toc), 0, 0).unwrap();
        assert_eq!(c.attendre_la_fin(), Some(Fin::Termine));

        let (code, v) = appel(router(e.clone()), "POST", "/ejecter", None).await;
        assert_eq!(code, StatusCode::OK, "{v}");
        assert_eq!(v["zones_arretees"], json!([]));
        assert_eq!(v["lecture_continue"], json!([3]));
        assert_eq!(l.ejections(), 1);
        assert!(hote.arrets.lock().await.is_empty(), "la zone joue encore");
        assert!(e.zones.lock().await.contains(&3));
        assert!(m.disque_complet().is_some(), "mémoire gardée");
        m.liberer();
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
