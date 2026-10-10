//! Les routes de l'extraction (#2466), montées avec celles du greffon sous
//! `/api/v1/ext/cd`. Toutes exigent un administrateur quand
//! l'authentification est active (même règle que `RequireAdmin` du serveur).
//!
//! * `GET    /lecteurs`             — les lecteurs et leur état ;
//! * `GET    /extraction/reglages`  — formats, destination, emplacements ;
//! * `PUT    /extraction/reglages`  — `{ "format", "destination" }` ;
//! * `POST   /extractions`          — lancer (voir [`DemandeExtraction`]) ;
//! * `GET    /extractions`          — les extractions connues ;
//! * `GET    /extractions/{id}`     — suivre une extraction ;
//! * `DELETE /extractions/{id}`     — l'annuler.
//!
//! Le disque (`GET /disque`) et l'éjection (`POST /ejecter`) sont ceux du
//! greffon ; l'éjection et la lecture refusent pendant une extraction.

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::body::Bytes;
use axum::extract::{Path, Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use tune_core::db::settings_repo::SettingsRepo;
use tune_http_types::AuthUser;

use super::balises::{Balises, type_d_image};
use super::destination::{self, CLE_DESTINATION, CLE_FORMAT};
use super::travail::{Plan, PlanPiste, executer};
use super::{
    EVT_DEMARREE, EVT_PROGRESSION, EVT_TERMINEE, EtatPiste, EtatTravail, Extractions, Format,
    StatutPiste, StatutTravail, Travail, Verification,
};
use crate::discid::disc_id;
use crate::lecteur::{Presence, plateforme_prise_en_charge};
use crate::routes::{EtatRoutes, Refus, refus, toc_ou_refus, zones_qui_jouent_le_disque};

/// Longueur maximale d'un texte fourni (artiste, album, titre).
pub const TEXTE_MAX: usize = 200;
/// Taille maximale d'une pochette téléchargée.
pub const POCHETTE_MAX: usize = 10 * 1024 * 1024;

/// D'où vient la pochette : Cover Art Archive en production, un témoin en
/// test.
#[async_trait]
pub trait Pochettes: Send + Sync {
    async fn telecharger(&self, url: &str) -> Option<Vec<u8>>;
}

pub struct CoverArtArchive;

#[async_trait]
impl Pochettes for CoverArtArchive {
    async fn telecharger(&self, url: &str) -> Option<Vec<u8>> {
        let r = tune_core::http::client::shared()
            .get(url)
            .timeout(Duration::from_secs(15))
            .send()
            .await
            .ok()?;
        if !r.status().is_success()
            || r.content_length()
                .is_some_and(|l| l as usize > POCHETTE_MAX)
        {
            return None;
        }
        let octets = r.bytes().await.ok()?;
        (octets.len() <= POCHETTE_MAX && type_d_image(&octets).is_some()).then(|| octets.to_vec())
    }
}

#[derive(Clone)]
pub struct EtatExtraction {
    pub routes: EtatRoutes,
    pub ex: Arc<Extractions>,
}

pub fn router(routes: EtatRoutes, ex: Arc<Extractions>) -> Router<()> {
    Router::new()
        .route("/lecteurs", get(lecteurs))
        .route("/extraction/reglages", get(reglages).put(regler))
        .route("/extractions", get(liste).post(lancer))
        .route("/extractions/{id}", get(une).delete(annuler))
        .layer(axum::middleware::from_fn_with_state(
            ex.clone(),
            exiger_admin,
        ))
        .with_state(EtatExtraction { routes, ex })
}

fn reponse((code, motif, message): Refus) -> Response {
    refus(code, motif, message)
}

fn mauvaise(motif: &'static str, message: impl Into<String>) -> Refus {
    (StatusCode::BAD_REQUEST, motif, message.into())
}

// ─── Accès ───────────────────────────────────────────────────────────────

/// La règle de `RequireAdmin` : sans authentification, ouvert ; avec, un
/// jeton d'administrateur (la clé d'API ne porte pas de rôle : refusée).
pub fn verdict_admin(auth_active: bool, user: Option<&AuthUser>) -> Result<(), Refus> {
    if !auth_active {
        return Ok(());
    }
    match user {
        Some(u) if u.role == "admin" => Ok(()),
        Some(_) => Err((
            StatusCode::FORBIDDEN,
            "admin_requis",
            "L'extraction de CD est réservée aux administrateurs.".into(),
        )),
        None => Err((
            StatusCode::UNAUTHORIZED,
            "authentification_requise",
            "Connectez-vous avec un compte administrateur.".into(),
        )),
    }
}

async fn exiger_admin(State(ex): State<Arc<Extractions>>, req: Request, next: Next) -> Response {
    let auth_active = SettingsRepo::with_backend(ex.backend.clone())
        .get("auth_enabled")
        .ok()
        .flatten()
        .is_some_and(|v| v == "true");
    match verdict_admin(auth_active, req.extensions().get::<AuthUser>()) {
        Ok(()) => next.run(req).await,
        Err(r) => reponse(r),
    }
}

// ─── Lecteurs et réglages ────────────────────────────────────────────────

async fn lecteurs(State(e): State<EtatExtraction>) -> Json<Value> {
    let mut v = Vec::new();
    if let Some(l) = e.routes.lecteur.clone() {
        let chemin = l.chemin();
        let presence = tokio::task::spawn_blocking(move || l.presence())
            .await
            .unwrap_or(Presence::AucunLecteur);
        if presence != Presence::AucunLecteur {
            v.push(json!({
                "chemin": chemin,
                "presence": presence,
                "extraction_en_cours": e.ex.en_cours().map(|t| t.id.clone()),
            }));
        }
    }
    Json(json!({
        "plateforme_prise_en_charge": plateforme_prise_en_charge(),
        "lecteurs": v,
    }))
}

fn format_regle(ex: &Extractions) -> Format {
    SettingsRepo::with_backend(ex.backend.clone())
        .get(CLE_FORMAT)
        .ok()
        .flatten()
        .and_then(|f| Format::depuis(&f))
        .unwrap_or_default()
}

fn etat_des_reglages(ex: &Extractions) -> Value {
    let (destination, source) = match destination::par_defaut(ex.backend.clone()) {
        Some((p, s)) => (Some(p.to_string_lossy().to_string()), s),
        None => (None, "aucune"),
    };
    json!({
        "formats": ["flac", "wav"],
        "format": format_regle(ex),
        "verifications": ["doute", "toujours"],
        "destination": destination,
        "destination_source": source,
        "emplacements": destination::emplacements(ex.backend.as_ref()),
    })
}

async fn reglages(State(e): State<EtatExtraction>) -> Json<Value> {
    Json(etat_des_reglages(&e.ex))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DemandeReglages {
    format: Option<String>,
    /// `""` revient au défaut (le premier emplacement).
    destination: Option<String>,
}

async fn regler(State(e): State<EtatExtraction>, corps: Bytes) -> Response {
    let d: DemandeReglages = match lire_corps(&corps) {
        Ok(Some(d)) => d,
        Ok(None) => return reponse(mauvaise("corps_invalide", "Corps JSON attendu.")),
        Err(r) => return reponse(r),
    };
    let repo = SettingsRepo::with_backend(e.ex.backend.clone());
    if let Some(f) = &d.format {
        let Some(f) = Format::depuis(f) else {
            return reponse(format_refuse(f));
        };
        if let Err(m) = repo.set(CLE_FORMAT, f.extension()) {
            return reponse((StatusCode::INTERNAL_SERVER_ERROR, "reglage", m));
        }
    }
    if let Some(dest) = &d.destination {
        let r = if dest.trim().is_empty() {
            repo.delete(CLE_DESTINATION)
        } else {
            match destination::verifier(dest, &destination::emplacements(e.ex.backend.as_ref())) {
                Ok(p) => repo.set(CLE_DESTINATION, &p.to_string_lossy()),
                Err(r) => return reponse((StatusCode::BAD_REQUEST, r.motif, r.message)),
            }
        };
        if let Err(m) = r {
            return reponse((StatusCode::INTERNAL_SERVER_ERROR, "reglage", m));
        }
    }
    Json(etat_des_reglages(&e.ex)).into_response()
}

fn format_refuse(f: &str) -> Refus {
    mauvaise(
        "format_non_pris_en_charge",
        format!("Format « {f} » non pris en charge : flac ou wav."),
    )
}

/// Un corps JSON strict ; vide = `None`.
fn lire_corps<T: for<'de> Deserialize<'de>>(corps: &Bytes) -> Result<Option<T>, Refus> {
    if corps.iter().all(u8::is_ascii_whitespace) {
        return Ok(None);
    }
    serde_json::from_slice(corps)
        .map(Some)
        .map_err(|e| mauvaise("corps_invalide", format!("Corps JSON invalide : {e}")))
}

// ─── Lancer ──────────────────────────────────────────────────────────────

/// Le corps de `POST /extractions`. Tout est facultatif ; un champ inconnu
/// est refusé.
#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct DemandeExtraction {
    /// `flac` (défaut, ou le réglage) ou `wav`.
    pub format: Option<String>,
    /// Un dossier dans un emplacement de la bibliothèque (défaut : le
    /// réglage, sinon le premier emplacement).
    pub destination: Option<String>,
    /// Les pistes à extraire (défaut : toutes les pistes audio).
    pub pistes: Option<Vec<u8>>,
    /// `doute` (défaut) ou `toujours`.
    pub verification: Option<Verification>,
    /// Remplacer des fichiers existants (défaut : refuser).
    #[serde(default)]
    pub ecraser: bool,
    /// Corrections, prioritaires sur MusicBrainz.
    pub artiste: Option<String>,
    pub album: Option<String>,
    /// `{ "3": "Titre de la piste 3" }`.
    pub titres: Option<HashMap<String, String>>,
}

fn texte_valide(champ: &'static str, v: &str) -> Result<String, Refus> {
    let v = v.trim();
    if v.chars().count() > TEXTE_MAX || v.chars().any(char::is_control) {
        return Err(mauvaise(
            "champ_invalide",
            format!("« {champ} » : au plus {TEXTE_MAX} caractères, sans caractère de contrôle."),
        ));
    }
    Ok(v.to_string())
}

fn conflit(motif: &'static str, message: impl Into<String>, extra: Value) -> Response {
    let mut corps = json!({ "error": motif, "message": message.into() });
    if let (Some(o), Some(x)) = (corps.as_object_mut(), extra.as_object()) {
        o.extend(x.clone());
    }
    (StatusCode::CONFLICT, Json(corps)).into_response()
}

pub(crate) fn conflit_extraction(id: &str) -> Response {
    conflit(
        "extraction_en_cours",
        "Une extraction du disque est en cours.",
        json!({ "extraction_id": id }),
    )
}

async fn lancer(State(e): State<EtatExtraction>, corps: Bytes) -> Response {
    let d: DemandeExtraction = match lire_corps(&corps) {
        Ok(d) => d.unwrap_or_default(),
        Err(r) => return reponse(r),
    };
    if let Some(t) = e.ex.en_cours() {
        return conflit_extraction(&t.id);
    }
    let zones = zones_qui_jouent_le_disque(&e.routes).await;
    if !zones.is_empty() {
        return conflit(
            "lecture_en_cours",
            "Le disque est en lecture : arrêtez la lecture avant de l'extraire.",
            json!({ "zones": zones }),
        );
    }
    match preparer(&e, d).await {
        Ok((travail, plan, pochette)) => {
            if let Err(autre) = e.ex.inscrire(travail.clone()) {
                return conflit_extraction(&autre.id);
            }
            let etat = travail.etat();
            e.ex.publier(EVT_DEMARREE, &etat);
            tokio::spawn(derouler(e.ex.clone(), travail, plan, pochette));
            (StatusCode::ACCEPTED, Json(etat)).into_response()
        }
        Err(r) => r,
    }
}

/// Valide la demande et construit l'état initial et le plan. Le refus est
/// une réponse déjà formée : une seule demande, rien à optimiser.
#[allow(clippy::result_large_err)]
async fn preparer(
    e: &EtatExtraction,
    d: DemandeExtraction,
) -> Result<(Arc<Travail>, Plan, Option<String>), Response> {
    let format = match &d.format {
        Some(f) => Format::depuis(f).ok_or_else(|| reponse(format_refuse(f)))?,
        None => format_regle(&e.ex),
    };
    let artiste_demande = d
        .artiste
        .as_deref()
        .map(|v| texte_valide("artiste", v))
        .transpose()
        .map_err(reponse)?;
    let album_demande = d
        .album
        .as_deref()
        .map(|v| texte_valide("album", v))
        .transpose()
        .map_err(reponse)?;
    let mut titres: HashMap<u8, String> = HashMap::new();
    for (k, v) in d.titres.iter().flatten() {
        let n: u8 = k.trim().parse().map_err(|_| {
            reponse(mauvaise(
                "champ_invalide",
                format!("« titres » : « {k} » n'est pas un numéro de piste."),
            ))
        })?;
        titres.insert(n, texte_valide("titres", v).map_err(reponse)?);
    }
    let destination = match &d.destination {
        Some(dest) => {
            destination::verifier(dest, &destination::emplacements(e.ex.backend.as_ref()))
                .map_err(|r| reponse((StatusCode::BAD_REQUEST, r.motif, r.message)))?
        }
        None => {
            destination::par_defaut(e.ex.backend.clone())
                .ok_or_else(|| {
                    conflit(
                        "aucun_emplacement",
                        "La bibliothèque n'a aucun emplacement où ranger l'extraction.",
                        json!({}),
                    )
                })?
                .0
        }
    };

    let toc = toc_ou_refus(&e.routes).await.map_err(reponse)?;
    let Some(lecteur) = e.routes.lecteur.clone() else {
        return Err(reponse((
            StatusCode::NOT_FOUND,
            "aucun_lecteur",
            "Aucun lecteur de CD.".into(),
        )));
    };
    let generation = lecteur.generation_lecteur();
    let audio: Vec<u8> = toc.pistes_audio().map(|p| p.numero).collect();
    let choisies: Vec<u8> = match &d.pistes {
        Some(v) if v.is_empty() => {
            return Err(reponse(mauvaise("pistes_vides", "Aucune piste demandée.")));
        }
        Some(v) => {
            let voulues: BTreeSet<u8> = v.iter().copied().collect();
            if let Some(n) = voulues.iter().find(|n| !audio.contains(n)) {
                return Err(reponse(mauvaise(
                    "piste_inconnue",
                    format!("La piste {n} n'est pas une piste audio de ce disque."),
                )));
            }
            audio
                .iter()
                .copied()
                .filter(|n| voulues.contains(n))
                .collect()
        }
        None => audio.clone(),
    };
    if let Some(n) = titres.keys().find(|n| !choisies.contains(n)) {
        return Err(reponse(mauvaise(
            "piste_inconnue",
            format!("« titres » : la piste {n} n'est pas extraite."),
        )));
    }

    let disc = disc_id(&toc);
    let infos = e.routes.consultation.consulter(&disc).await;
    let artiste_album = artiste_demande
        .clone()
        .or_else(|| infos.as_ref().map(|i| i.artiste.clone()))
        .unwrap_or_default();
    let album = album_demande
        .or_else(|| infos.as_ref().map(|i| i.titre.clone()))
        .unwrap_or_default();
    let (disque, disques) = infos
        .as_ref()
        .map(|i| (i.disque, i.disques))
        .unwrap_or((1, 1));
    let premiere = audio.first().copied();
    let derniere = audio.last().copied();

    let mut pistes_plan = Vec::new();
    let mut pistes_etat = Vec::new();
    for &n in &choisies {
        let mb = infos.as_ref().and_then(|i| i.pistes.get(&n));
        let titre = titres
            .get(&n)
            .cloned()
            .or_else(|| mb.map(|m| m.titre.clone()))
            .unwrap_or_else(|| format!("Piste {n:02}"));
        // Un artiste corrigé par l'utilisateur vaut pour tout le disque.
        let artiste = if artiste_demande.is_some() {
            artiste_album.clone()
        } else {
            mb.and_then(|m| m.artiste.clone())
                .unwrap_or_else(|| artiste_album.clone())
        };
        let chemin = destination.join(destination::chemin_relatif(
            &artiste_album,
            &album,
            n,
            &titre,
            disque,
            disques,
            format.extension(),
        ));
        let (debut, fin) = match (toc.piste(n), toc.fin_de_piste(n)) {
            (Some(p), Some(f)) => (p.debut, f),
            _ => continue,
        };
        let mb_ids = |piste: &[String]| -> Vec<String> {
            if artiste_demande.is_some() {
                Vec::new()
            } else if !piste.is_empty() {
                piste.to_vec()
            } else {
                infos
                    .as_ref()
                    .map(|i| i.artiste_ids.clone())
                    .unwrap_or_default()
            }
        };
        let balises = Balises {
            titre: titre.clone(),
            artiste,
            album: album.clone(),
            artiste_album: artiste_album.clone(),
            numero: n as u32,
            total_pistes: audio.len() as u32,
            disque,
            disques,
            date: infos.as_ref().and_then(|i| i.date.clone()),
            release_id: infos.as_ref().and_then(|i| i.release_id.clone()),
            recording_id: mb.and_then(|m| m.recording_id.clone()),
            piste_id: mb.and_then(|m| m.piste_id.clone()),
            artiste_ids: mb_ids(mb.map(|m| m.artiste_ids.as_slice()).unwrap_or(&[])),
            artiste_album_ids: mb_ids(&[]),
        };
        pistes_etat.push(EtatPiste {
            numero: n,
            titre,
            statut: StatutPiste::EnAttente,
            secteurs: fin - debut,
            secteurs_lus: 0,
            pourcentage: 0.0,
            lectures_supplementaires: 0,
            secteurs_illisibles: 0,
            accuraterip_v1: None,
            accuraterip_v2: None,
            fichier: None,
            erreur: None,
        });
        pistes_plan.push(PlanPiste {
            numero: n,
            debut,
            fin,
            chemin,
            balises,
            premiere_audio: Some(n) == premiere,
            derniere_audio: Some(n) == derniere,
        });
    }
    if !d.ecraser {
        let existants: Vec<String> = pistes_plan
            .iter()
            .filter(|p| p.chemin.exists())
            .map(|p| p.chemin.to_string_lossy().to_string())
            .collect();
        if !existants.is_empty() {
            return Err(conflit(
                "fichiers_existants",
                "Des fichiers de cette extraction existent déjà : confirmez pour les remplacer.",
                json!({ "fichiers": existants }),
            ));
        }
    }
    let dossier = pistes_plan
        .first()
        .and_then(|p| p.chemin.parent())
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| destination.clone());
    let verification = d.verification.unwrap_or_default();
    let mut etat = EtatTravail {
        id: uuid::Uuid::new_v4().simple().to_string()[..12].to_string(),
        statut: StatutTravail::EnCours,
        format,
        verification,
        lecteur: lecteur.chemin(),
        disc_id: disc,
        metadonnees: if infos.is_some() {
            "musicbrainz"
        } else {
            "repli"
        },
        artiste: artiste_album,
        album,
        disque,
        disques,
        destination: destination.to_string_lossy().to_string(),
        dossier: dossier.to_string_lossy().to_string(),
        pistes: pistes_etat,
        piste_courante: None,
        pourcentage: 0.0,
        debut: maintenant(),
        fin: None,
        erreur: None,
        scan: None,
    };
    etat.recalculer();
    let plan = Plan {
        lecteur,
        generation,
        format,
        verification,
        ecraser: d.ecraser,
        dossier,
        pistes: pistes_plan,
        pochette: None,
    };
    let pochette = infos.and_then(|i| i.pochette);
    Ok((Arc::new(Travail::new(etat)), plan, pochette))
}

fn maintenant() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Le déroulé d'une extraction lancée : pochette, fil bloquant, puis scan
/// ciblé et évènement de fin.
pub(crate) async fn derouler(
    ex: Arc<Extractions>,
    travail: Arc<Travail>,
    mut plan: Plan,
    pochette: Option<String>,
) {
    if let Some(url) = pochette {
        plan.pochette = ex.pochettes.telecharger(&url).await;
    }
    let (ex2, t2) = (ex.clone(), travail.clone());
    let resultat = tokio::task::spawn_blocking(move || {
        let publier = || ex2.publier(EVT_PROGRESSION, &t2.etat());
        executer(&plan, &t2, &publier)
    })
    .await;
    let ecrites = travail.modifier(|e| {
        e.pistes
            .iter()
            .filter(|p| p.statut == StatutPiste::Terminee)
            .count()
    });
    let scan = if ecrites == 0 {
        "non_lance"
    } else if let Some(s) = &ex.scan {
        if s.scanner(travail.etat().dossier).await {
            "lance"
        } else {
            "deja_en_cours"
        }
    } else {
        "indisponible"
    };
    let etat = travail.modifier(|e| {
        match resultat {
            Ok(Ok(())) => e.statut = StatutTravail::Terminee,
            Ok(Err(err)) if err.code == "annulee" => e.statut = StatutTravail::Annulee,
            Ok(Err(err)) => {
                e.statut = StatutTravail::Echec;
                e.erreur = Some(err);
            }
            Err(join) => {
                e.statut = StatutTravail::Echec;
                e.erreur = Some(super::ErreurTravail::new("interne", join.to_string()));
            }
        }
        e.scan = Some(scan);
        e.fin = Some(maintenant());
        e.piste_courante = None;
        e.recalculer();
        e.clone()
    });
    tracing::info!(
        id = %etat.id,
        statut = ?etat.statut,
        pistes = ecrites,
        scan,
        "cd_extraction_finie"
    );
    ex.publier(EVT_TERMINEE, &etat);
}

// ─── Suivre, annuler ─────────────────────────────────────────────────────

async fn liste(State(e): State<EtatExtraction>) -> Json<Value> {
    Json(json!({ "extractions": e.ex.liste() }))
}

fn inconnue(id: &str) -> Response {
    reponse((
        StatusCode::NOT_FOUND,
        "extraction_inconnue",
        format!("Aucune extraction « {id} »."),
    ))
}

async fn une(State(e): State<EtatExtraction>, Path(id): Path<String>) -> Response {
    match e.ex.trouver(&id) {
        Some(t) => Json(t.etat()).into_response(),
        None => inconnue(&id),
    }
}

async fn annuler(State(e): State<EtatExtraction>, Path(id): Path<String>) -> Response {
    let Some(t) = e.ex.trouver(&id) else {
        return inconnue(&id);
    };
    if !t.en_cours() {
        return conflit(
            "extraction_terminee",
            "Cette extraction est déjà terminée.",
            json!({ "statut": t.etat().statut }),
        );
    }
    t.annuler();
    tracing::info!(id = %id, "cd_extraction_annulation_demandee");
    (StatusCode::ACCEPTED, Json(t.etat())).into_response()
}
