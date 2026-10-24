//! Les routes HTTP du greffon. Voir le tableau de [`crate`].

use axum::extract::{Path, Request, State};
use axum::handler::Handler;
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::{info, warn};
use tune_core::db::settings_repo::SettingsRepo;
use tune_http_types::AuthUser;

use crate::appel::{self, ErreurAppel};
use crate::pistes::{self, EtatPistes, LignePiste};
use crate::reglages::{self, Modification};
use crate::regles::{self, Cible, Referentiel};
use crate::suggestions::{self, EtatAlbum, LigneAAppliquer};
use crate::{Etat, cache, couts};

/// Albums d'un lot de suggestions, au plus.
pub const LOT_MAX: usize = 50;
/// Albums d'un lot d'application ou d'annulation, au plus.
pub const LOT_APPLICATION_MAX: usize = 200;

pub fn router_vers(etat: Etat) -> Router<()> {
    // Étape 2 : changer les réglages et « Tester la clé » (qui dépense) sont
    // réservés aux administrateurs. La garde est posée sur ces deux
    // gestionnaires seulement : lire les réglages reste ouvert (l'écran doit
    // savoir si l'assistant est configuré), avec `modifiable`.
    let garde = || middleware::from_fn_with_state(etat.clone(), exiger_admin);
    Router::new()
        .route(
            "/reglages",
            get(lire_reglages).put(changer_reglages.layer(garde())),
        )
        .route("/tester", post(tester.layer(garde())))
        .route("/playlist", post(playlist))
        .route("/albums/{id}/suggestions", post(suggerer))
        .route("/albums/{id}/appliquer", post(appliquer))
        .route("/albums/{id}/annuler", post(annuler))
        .route("/albums/{id}/historique", get(historique))
        .route("/albums/{id}/pistes/suggestions", post(suggerer_pistes))
        .route("/albums/{id}/pistes/appliquer", post(appliquer_pistes))
        .route("/lot/suggestions", post(lot_suggestions))
        .route("/lot/appliquer", post(lot_appliquer))
        .route("/lot/annuler", post(lot_annuler))
        .route("/collections/affiner", post(collections_affiner))
        .route("/collections/proposer", post(collections_proposer))
        .route("/collections/nommer", post(collections_nommer))
        .route("/collections/pourquoi", post(collections_pourquoi))
        .route("/collections/ecoute", post(collections_ecoute))
        .route("/diagnostic/apercu", post(diagnostic_apercu))
        .route("/diagnostic", post(diagnostic_envoyer))
        .with_state(etat)
}

fn erreur(statut: StatusCode, code: &str, message: impl Into<String>) -> Response {
    (
        statut,
        Json(json!({ "ok": false, "error": code, "message": message.into() })),
    )
        .into_response()
}

fn interne(e: impl std::fmt::Display) -> Response {
    warn!(erreur = %e, "assistant_ia_erreur_interne");
    erreur(StatusCode::INTERNAL_SERVER_ERROR, "interne", e.to_string())
}

/// La langue de la réponse : celle demandée, sinon celle de l'interface.
fn langue(demandee: Option<&str>, entetes: &HeaderMap) -> String {
    let brute = demandee
        .map(String::from)
        .or_else(|| {
            entetes
                .get(axum::http::header::ACCEPT_LANGUAGE)
                .and_then(|v| v.to_str().ok())
                .map(String::from)
        })
        .unwrap_or_default();
    let code: String = brute
        .chars()
        .take_while(|c| c.is_ascii_alphabetic())
        .take(3)
        .collect::<String>()
        .to_ascii_lowercase();
    if code.len() >= 2 { code } else { "fr".into() }
}

// ---------------------------------------------------------------------------
// Accès : les réglages aux administrateurs
// ---------------------------------------------------------------------------

/// La règle de `RequireAdmin` (`tune-server/src/auth.rs`) : sans
/// authentification, ouvert ; avec, un jeton d'administrateur. La clé d'API
/// du serveur ne porte pas de rôle : refusée. Jamais 401 : le client web y
/// lit une session expirée et déconnecte.
pub fn verdict_admin(auth_active: bool, user: Option<&AuthUser>) -> bool {
    !auth_active || user.is_some_and(|u| u.role == "admin")
}

fn refus_admin() -> Response {
    erreur(
        StatusCode::FORBIDDEN,
        "admin_requis",
        "Les réglages de l'assistant IA sont réservés aux administrateurs.",
    )
}

fn auth_active(etat: &Etat) -> bool {
    SettingsRepo::with_backend(etat.backend.clone())
        .get("auth_enabled")
        .ok()
        .flatten()
        .is_some_and(|v| v == "true")
}

async fn exiger_admin(State(etat): State<Etat>, req: Request, next: Next) -> Response {
    if verdict_admin(auth_active(&etat), req.extensions().get::<AuthUser>()) {
        next.run(req).await
    } else {
        refus_admin()
    }
}

// ---------------------------------------------------------------------------
// Réglages
// ---------------------------------------------------------------------------

fn vue_reglages(etat: &Etat, modifiable: bool) -> Value {
    let mut v = reglages::vue(&etat.backend, &etat.coffre);
    v["modifiable"] = json!(modifiable);
    v
}

async fn lire_reglages(State(etat): State<Etat>, req: Request) -> Response {
    let modifiable = verdict_admin(auth_active(&etat), req.extensions().get::<AuthUser>());
    Json(vue_reglages(&etat, modifiable)).into_response()
}

async fn changer_reglages(State(etat): State<Etat>, Json(m): Json<Modification>) -> Response {
    match reglages::appliquer(&etat.backend, &etat.coffre, &m) {
        Ok(()) => {
            info!(
                cle_changee = m.cle.is_some(),
                "assistant_ia_reglages_changes"
            );
            Json(vue_reglages(&etat, true)).into_response()
        }
        Err(e) => erreur(StatusCode::BAD_REQUEST, "reglage_invalide", e),
    }
}

/// « Tester la clé » : un appel réel, minuscule, hors cache.
async fn tester(State(etat): State<Etat>) -> Response {
    let r = appel::appeler(
        &etat,
        "test",
        false,
        "Reply with the JSON object {\"ok\": true} and nothing else.",
        "ping",
        256,
        None,
    )
    .await;
    match r {
        Ok(res) => Json(json!({
            "ok": true,
            "cout_eur": couts::micro_en_euros(res.cout_micro_eur),
            "plafond": appel::etat_plafond(&etat.backend),
        }))
        .into_response(),
        Err(e) => appel::reponse_erreur(&e),
    }
}

// ---------------------------------------------------------------------------
// Playlist par phrase
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct DemandePlaylist {
    phrase: String,
    cible: Option<String>,
    langue: Option<String>,
}

/// Les genres les plus fréquents de la bibliothèque : le modèle les reprend
/// tels qu'ils sont écrits, au lieu d'en inventer l'orthographe.
fn genres_de_la_bibliotheque(etat: &Etat) -> Vec<String> {
    etat.backend
        .query_many(
            "SELECT genre, COUNT(*) AS n FROM albums WHERE genre IS NOT NULL AND genre <> '' \
             GROUP BY genre ORDER BY n DESC LIMIT 60",
            &[],
        )
        .map(|lignes| {
            lignes
                .iter()
                .filter_map(|l| l.first().and_then(|v| v.as_string()))
                .collect()
        })
        .unwrap_or_default()
}

async fn playlist(
    State(etat): State<Etat>,
    entetes: HeaderMap,
    Json(d): Json<DemandePlaylist>,
) -> Response {
    let phrase = cache::normaliser_phrase(&d.phrase);
    if phrase.is_empty() || phrase.chars().count() > 500 {
        return erreur(
            StatusCode::BAD_REQUEST,
            "phrase_invalide",
            "Décrivez la playlist en une phrase (500 caractères au plus).",
        );
    }
    let cible = Cible::depuis(d.cible.as_deref());
    let langue = langue(d.langue.as_deref(), &entetes);
    // Étape 2 : les noms des étiquettes et des collections existantes.
    let refs = Referentiel::charger(&etat.backend);
    let offerts = regles::criteres_offerts(&etat.criteres, cible, &refs);
    let genres = genres_de_la_bibliotheque(&etat);
    let systeme = regles::consigne(&offerts, cible, &genres, &refs, &langue);
    // La consigne entière fait partie de la clé : de nouveaux genres, un
    // critère ajouté au moteur, une étiquette créée, une autre langue — une
    // autre question.
    let entree = format!("{}\n{}\n{}", cible.nom(), systeme, phrase);
    let res = match appel::appeler(
        &etat,
        "playlist",
        false,
        &systeme,
        &d.phrase,
        4096,
        Some(&entree),
    )
    .await
    {
        Ok(r) => r,
        Err(e) => return appel::reponse_erreur(&e),
    };
    let Some(brut) = appel::objet_json(&res.texte) else {
        return erreur(
            StatusCode::UNPROCESSABLE_ENTITY,
            "reponse_invalide",
            "Le modèle n'a pas rendu un objet JSON.",
        );
    };
    let recues = brut.get("rules").and_then(Value::as_array);
    match regles::valider(&brut, &offerts, cible, &refs) {
        // #6731 — une proposition sans AUCUNE règle exploitable n'est pas une
        // proposition : enregistrée telle quelle, elle rendrait toute la
        // bibliothèque. On la refuse, en disant pourquoi (les règles écartées
        // et ce que le modèle n'a pas su traduire), au lieu de la rendre `ok`.
        Ok(p) if p.rules.is_empty() => {
            warn!(
                cible = cible.nom(),
                recues = recues.map(Vec::len).unwrap_or(0),
                regles_recues = %regles::resume_pour_journal(recues.map(Vec::as_slice).unwrap_or(&[])),
                ecartees = %regles::resume_des_refusees(&p.refusees),
                non_traduit = p.untranslated.len(),
                "assistant_ia_playlist_sans_regle"
            );
            (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(json!({
                    "ok": false,
                    "error": "aucune_regle",
                    "message": "Le modèle n'a proposé aucune règle utilisable : rien n'est créé. Reformulez avec un critère concret (genre, artiste, années, durée…).",
                    "refusees": p.refusees,
                    "untranslated": p.untranslated,
                })),
            )
                .into_response()
        }
        Ok(p) => {
            info!(
                cible = cible.nom(),
                recues = recues.map(Vec::len).unwrap_or(0),
                retenues = p.rules.len(),
                regles_retenues = %regles::resume_pour_journal(&p.rules),
                ecartees = %regles::resume_des_refusees(&p.refusees),
                "assistant_ia_playlist_regles"
            );
            Json(json!({
                "ok": true,
                "cible": cible.nom(),
                "proposition": p,
                "depuis_cache": res.depuis_cache,
                "cout_eur": couts::micro_en_euros(res.cout_micro_eur),
                "plafond": appel::etat_plafond(&etat.backend),
            }))
            .into_response()
        }
        Err(code) => {
            warn!(
                cible = cible.nom(),
                code, "assistant_ia_playlist_reponse_hors_schema"
            );
            erreur(
                StatusCode::UNPROCESSABLE_ENTITY,
                code,
                "La réponse du modèle ne suit pas le schéma demandé.",
            )
        }
    }
}

// ---------------------------------------------------------------------------
// Suggestions sur un album, sur ses pistes, sur un lot
// ---------------------------------------------------------------------------

/// Pourquoi une suggestion n'a pas abouti.
enum Echec {
    /// L'appel n'a pas eu lieu ou a échoué : plafond, clé, fournisseur. Il
    /// échouerait de même pour l'album suivant : un LOT s'arrête net.
    Appel(ErreurAppel),
    Introuvable,
    ReponseInvalide(&'static str),
    Interne(String),
}

impl Echec {
    fn code(&self) -> &'static str {
        match self {
            Self::Appel(e) => e.code(),
            Self::Introuvable => "album_introuvable",
            Self::ReponseInvalide(c) => c,
            Self::Interne(_) => "interne",
        }
    }

    fn message(&self) -> String {
        match self {
            Self::Appel(e) => e.message(),
            Self::Introuvable => "Album introuvable.".into(),
            Self::ReponseInvalide(_) => {
                "La réponse du modèle ne suit pas le schéma demandé.".into()
            }
            Self::Interne(e) => e.clone(),
        }
    }

    fn reponse(&self) -> Response {
        match self {
            Self::Appel(e) => appel::reponse_erreur(e),
            Self::Introuvable => erreur(StatusCode::NOT_FOUND, self.code(), self.message()),
            Self::ReponseInvalide(_) => erreur(
                StatusCode::UNPROCESSABLE_ENTITY,
                self.code(),
                self.message(),
            ),
            Self::Interne(e) => interne(e),
        }
    }
}

/// Une suggestion aboutie : le corps de la réponse et ce qu'elle a coûté.
struct Aboutie {
    corps: Value,
    cout_micro_eur: u64,
}

async fn suggestions_d_un_album(etat: &Etat, id: i64, langue: &str) -> Result<Aboutie, Echec> {
    let etat_album = EtatAlbum::charger(&etat.backend, id)
        .map_err(Echec::Interne)?
        .ok_or(Echec::Introuvable)?;
    let systeme = suggestions::consigne(langue);
    let contexte = etat_album.contexte().to_string();
    let entree = format!("{systeme}\n{contexte}");
    let classique = etat_album.semble_classique();
    let res = appel::appeler(
        etat,
        "album",
        classique,
        &systeme,
        &contexte,
        4096,
        Some(&entree),
    )
    .await
    .map_err(Echec::Appel)?;
    let brut = appel::objet_json(&res.texte).ok_or(Echec::ReponseInvalide("reponse_invalide"))?;
    // L'état est RELU ici : une réponse du cache est jugée sur la base
    // d'aujourd'hui, pas sur celle du jour où elle a été payée.
    let etat_album = EtatAlbum::charger(&etat.backend, id)
        .map_err(Echec::Interne)?
        .ok_or(Echec::Introuvable)?;
    let (retenues, ecartees) =
        suggestions::evaluer(&brut, &etat_album).map_err(Echec::ReponseInvalide)?;
    Ok(Aboutie {
        corps: json!({
            "ok": true,
            "album_id": id,
            "titre": etat_album.valeur("title"),
            "suggestions": retenues,
            "ecartees": ecartees,
            "depuis_cache": res.depuis_cache,
            "cout_eur": couts::micro_en_euros(res.cout_micro_eur),
        }),
        cout_micro_eur: res.cout_micro_eur,
    })
}

#[derive(Deserialize, Default)]
struct DemandeSuggestions {
    langue: Option<String>,
}

async fn suggerer(
    State(etat): State<Etat>,
    Path(id): Path<i64>,
    entetes: HeaderMap,
    corps: Option<Json<DemandeSuggestions>>,
) -> Response {
    let d = corps.map(|Json(d)| d).unwrap_or_default();
    let langue = langue(d.langue.as_deref(), &entetes);
    match suggestions_d_un_album(&etat, id, &langue).await {
        Ok(mut a) => {
            a.corps["plafond"] = appel::etat_plafond(&etat.backend);
            Json(a.corps).into_response()
        }
        Err(e) => e.reponse(),
    }
}

#[derive(Deserialize)]
struct DemandeApplication {
    lignes: Vec<LigneAAppliquer>,
}

async fn appliquer(
    State(etat): State<Etat>,
    Path(id): Path<i64>,
    Json(d): Json<DemandeApplication>,
) -> Response {
    let _verrou = etat.verrou_albums.lock().await;
    match suggestions::appliquer(&etat.backend, id, &d.lignes) {
        Ok(Some(r)) => {
            info!(
                album_id = id,
                appliquees = r.appliquees.len(),
                refusees = r.refusees.len(),
                pistes_genre = r.pistes_genre,
                "assistant_ia_suggestions_appliquees"
            );
            Json(json!({ "ok": true, "rapport": r })).into_response()
        }
        Ok(None) => Echec::Introuvable.reponse(),
        Err(e) => interne(e),
    }
}

#[derive(Deserialize, Default)]
struct DemandeAnnulation {
    operation_id: Option<String>,
}

async fn annuler(
    State(etat): State<Etat>,
    Path(id): Path<i64>,
    corps: Option<Json<DemandeAnnulation>>,
) -> Response {
    let d = corps.map(|Json(d)| d).unwrap_or_default();
    let _verrou = etat.verrou_albums.lock().await;
    match suggestions::annuler(&etat.backend, id, d.operation_id.as_deref()) {
        Ok(Some(r)) => Json(json!({ "ok": true, "rapport": r })).into_response(),
        Ok(None) => Echec::Introuvable.reponse(),
        Err(e) => interne(e),
    }
}

async fn historique(State(etat): State<Etat>, Path(id): Path<i64>) -> Response {
    match suggestions::journal(&etat.backend, id) {
        Ok(ops) => Json(json!({ "ok": true, "operations": ops })).into_response(),
        Err(e) => interne(e),
    }
}

// --- Pistes ---------------------------------------------------------------

async fn suggerer_pistes(
    State(etat): State<Etat>,
    Path(id): Path<i64>,
    entetes: HeaderMap,
    corps: Option<Json<DemandeSuggestions>>,
) -> Response {
    let d = corps.map(|Json(d)| d).unwrap_or_default();
    let langue = langue(d.langue.as_deref(), &entetes);
    let etat_pistes = match EtatPistes::charger(&etat.backend, id) {
        Ok(Some(e)) => e,
        Ok(None) => return Echec::Introuvable.reponse(),
        Err(e) => return interne(e),
    };
    if etat_pistes.pistes.is_empty() {
        return Json(json!({
            "ok": true, "album_id": id, "suggestions": [], "ecartees": [],
            "depuis_cache": false, "cout_eur": 0.0,
        }))
        .into_response();
    }
    let systeme = pistes::consigne(&langue);
    let contexte = etat_pistes.contexte().to_string();
    let entree = format!("{systeme}\n{contexte}");
    let res = match appel::appeler(
        &etat,
        "pistes",
        etat_pistes.album.semble_classique(),
        &systeme,
        &contexte,
        8192,
        Some(&entree),
    )
    .await
    {
        Ok(r) => r,
        Err(e) => return appel::reponse_erreur(&e),
    };
    let Some(brut) = appel::objet_json(&res.texte) else {
        return Echec::ReponseInvalide("reponse_invalide").reponse();
    };
    // Relu, comme pour l'album : la garde juge l'état présent.
    let etat_pistes = match EtatPistes::charger(&etat.backend, id) {
        Ok(Some(e)) => e,
        Ok(None) => return Echec::Introuvable.reponse(),
        Err(e) => return interne(e),
    };
    match pistes::evaluer(&brut, &etat_pistes) {
        Ok((retenues, ecartees)) => Json(json!({
            "ok": true,
            "album_id": id,
            "suggestions": retenues,
            "ecartees": ecartees,
            "depuis_cache": res.depuis_cache,
            "cout_eur": couts::micro_en_euros(res.cout_micro_eur),
            "plafond": appel::etat_plafond(&etat.backend),
        }))
        .into_response(),
        Err(code) => Echec::ReponseInvalide(code).reponse(),
    }
}

#[derive(Deserialize)]
struct DemandeApplicationPistes {
    lignes: Vec<LignePiste>,
}

async fn appliquer_pistes(
    State(etat): State<Etat>,
    Path(id): Path<i64>,
    Json(d): Json<DemandeApplicationPistes>,
) -> Response {
    let _verrou = etat.verrou_albums.lock().await;
    match pistes::appliquer(&etat.backend, id, &d.lignes) {
        Ok(Some(r)) => {
            info!(
                album_id = id,
                appliquees = r.appliquees.len(),
                refusees = r.refusees.len(),
                "assistant_ia_suggestions_pistes_appliquees"
            );
            Json(json!({ "ok": true, "rapport": r })).into_response()
        }
        Ok(None) => Echec::Introuvable.reponse(),
        Err(e) => interne(e),
    }
}

// --- Lot ------------------------------------------------------------------

#[derive(Deserialize)]
struct DemandeLot {
    album_ids: Vec<i64>,
    langue: Option<String>,
    /// Plafond de CE lot, en euros, en plus du plafond mensuel.
    budget_eur: Option<Value>,
}

fn euros(v: &Value) -> Option<u64> {
    match v {
        Value::Null => None,
        Value::String(s) if s.trim().is_empty() => None,
        Value::String(s) => couts::euros_en_micro(s),
        autre => couts::euros_en_micro(&autre.to_string()),
    }
}

/// Un aperçu sur un lot d'albums : les albums sont traités UN PAR UN, chacun
/// par le chemin de l'album seul (cache, plafond, garde). Le lot s'arrête
/// NET à la première erreur d'appel — plafond mensuel atteint, clé refusée,
/// quota : elle frapperait l'album suivant de même — ou quand ce qu'il a
/// coûté atteint `budget_eur`. Les albums non traités sont rendus dans
/// `non_traites`. Rien n'est écrit : l'écran applique ensuite par
/// `/lot/appliquer`.
async fn lot_suggestions(
    State(etat): State<Etat>,
    entetes: HeaderMap,
    Json(d): Json<DemandeLot>,
) -> Response {
    let mut ids: Vec<i64> = Vec::new();
    for id in d.album_ids {
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    if ids.is_empty() || ids.len() > LOT_MAX {
        return erreur(
            StatusCode::BAD_REQUEST,
            "lot_invalide",
            format!("Un lot compte de 1 à {LOT_MAX} albums."),
        );
    }
    let budget = match &d.budget_eur {
        None => None,
        Some(v) => match (v, euros(v)) {
            (Value::Null, _) => None,
            (Value::String(s), _) if s.trim().is_empty() => None,
            (_, Some(b)) => Some(b),
            (_, None) => {
                return erreur(
                    StatusCode::BAD_REQUEST,
                    "lot_invalide",
                    "Budget du lot illisible.",
                );
            }
        },
    };
    let langue = langue(d.langue.as_deref(), &entetes);
    let mut resultats = Vec::new();
    let mut total = 0u64;
    let mut arret: Option<Value> = None;
    let mut non_traites: Vec<i64> = Vec::new();
    for (i, id) in ids.iter().enumerate() {
        if let Some(b) = budget
            && total >= b
        {
            arret = Some(json!({
                "error": "budget_du_lot_atteint",
                "message": "Le budget de ce lot est atteint : les albums restants n'ont pas été soumis.",
            }));
            non_traites = ids[i..].to_vec();
            break;
        }
        match suggestions_d_un_album(&etat, *id, &langue).await {
            Ok(a) => {
                total += a.cout_micro_eur;
                resultats.push(a.corps);
            }
            Err(e @ Echec::Appel(_)) => {
                info!(code = e.code(), traites = i, "assistant_ia_lot_arrete");
                arret = Some(json!({ "error": e.code(), "message": e.message() }));
                non_traites = ids[i..].to_vec();
                break;
            }
            Err(e) => resultats.push(json!({
                "ok": false, "album_id": id, "error": e.code(), "message": e.message(),
            })),
        }
    }
    Json(json!({
        "ok": true,
        "resultats": resultats,
        "arret": arret,
        "non_traites": non_traites,
        "cout_eur": couts::micro_en_euros(total),
        "plafond": appel::etat_plafond(&etat.backend),
    }))
    .into_response()
}

#[derive(Deserialize)]
struct AlbumDuLot {
    album_id: i64,
    lignes: Vec<LigneAAppliquer>,
}

#[derive(Deserialize)]
struct DemandeLotApplication {
    albums: Vec<AlbumDuLot>,
}

async fn lot_appliquer(State(etat): State<Etat>, Json(d): Json<DemandeLotApplication>) -> Response {
    if d.albums.len() > LOT_APPLICATION_MAX {
        return erreur(
            StatusCode::BAD_REQUEST,
            "lot_invalide",
            format!("Un lot compte au plus {LOT_APPLICATION_MAX} albums."),
        );
    }
    let _verrou = etat.verrou_albums.lock().await;
    let mut rapports = Vec::new();
    for a in &d.albums {
        rapports.push(match suggestions::appliquer(&etat.backend, a.album_id, &a.lignes) {
            Ok(Some(r)) => json!({ "ok": true, "album_id": a.album_id, "rapport": r }),
            Ok(None) => json!({ "ok": false, "album_id": a.album_id, "error": "album_introuvable" }),
            Err(e) => {
                warn!(album_id = a.album_id, erreur = %e, "assistant_ia_lot_application_echouee");
                json!({ "ok": false, "album_id": a.album_id, "error": "interne" })
            }
        });
    }
    info!(albums = d.albums.len(), "assistant_ia_lot_applique");
    Json(json!({ "ok": true, "rapports": rapports })).into_response()
}

#[derive(Deserialize)]
struct OperationDuLot {
    album_id: i64,
    operation_id: Option<String>,
}

#[derive(Deserialize)]
struct DemandeLotAnnulation {
    operations: Vec<OperationDuLot>,
}

async fn lot_annuler(State(etat): State<Etat>, Json(d): Json<DemandeLotAnnulation>) -> Response {
    if d.operations.len() > LOT_APPLICATION_MAX {
        return erreur(
            StatusCode::BAD_REQUEST,
            "lot_invalide",
            format!("Un lot compte au plus {LOT_APPLICATION_MAX} albums."),
        );
    }
    let _verrou = etat.verrou_albums.lock().await;
    let mut rapports = Vec::new();
    for o in &d.operations {
        rapports.push(
            match suggestions::annuler(&etat.backend, o.album_id, o.operation_id.as_deref()) {
                Ok(Some(r)) => json!({ "ok": true, "album_id": o.album_id, "rapport": r }),
                Ok(None) => {
                    json!({ "ok": false, "album_id": o.album_id, "error": "album_introuvable" })
                }
                Err(e) => {
                    warn!(album_id = o.album_id, erreur = %e, "assistant_ia_lot_annulation_echouee");
                    json!({ "ok": false, "album_id": o.album_id, "error": "interne" })
                }
            },
        );
    }
    Json(json!({ "ok": true, "rapports": rapports })).into_response()
}

/// Le coût ESTIMÉ d'un appel, avant son envoi, au tarif du modèle choisi
/// (décision du 23/10/2026 : coût estimé avant l'appel, montré avec l'aperçu
/// de ce qui part). `None` : tarif inconnu ou assistant non configuré —
/// l'appel serait refusé.
pub(crate) fn cout_estime_eur(
    etat: &Etat,
    modele: &str,
    systeme: &str,
    message: &str,
    max_sortie: u32,
) -> Option<f64> {
    let r = reglages::Reglages::charger(&etat.backend);
    let tarif = r.tarif(modele)?;
    Some(couts::micro_en_euros(couts::estimer_micro_eur(
        tarif,
        &format!("{systeme}{message}"),
        max_sortie,
        r.taux_usd_eur,
    )))
}

// ---------------------------------------------------------------------------
// Collections intelligentes (#6839)
// ---------------------------------------------------------------------------

#[derive(Deserialize, Default)]
struct DemandeCollection {
    cible: Option<String>,
    langue: Option<String>,
    /// (1) La liste à affiner : `{name, match_mode, rules}`.
    actuelle: Option<Value>,
    /// (1) Ce que l'utilisateur veut changer.
    phrase: Option<String>,
    /// (3), (4) Les règles à nommer ou à expliquer.
    rules: Option<Vec<Value>>,
    match_mode: Option<String>,
    /// (4) L'album à expliquer.
    album_id: Option<i64>,
    /// (5) Le profil qui demande : ses écoutes seulement.
    profil_id: Option<i64>,
    /// Rien n'est envoyé : ce qui partirait, et son coût estimé.
    #[serde(default)]
    apercu: bool,
}

/// Un appel d'un usage de collections : l'aperçu (rien ne part), ou l'appel
/// compté et son objet JSON. `Err` : la réponse HTTP à rendre telle quelle.
async fn appel_collection(
    etat: &Etat,
    usage: &str,
    systeme: &str,
    message: &str,
    max_jetons: u32,
    apercu: bool,
) -> Result<(Value, appel::Resultat), Response> {
    let modele = reglages::Reglages::charger(&etat.backend).modele_choisi(appel::Choix::Defaut);
    if apercu {
        return Err(Json(json!({
            "ok": true,
            "apercu": true,
            "modele": modele,
            "envoye": { "systeme": systeme, "message": message },
            "cout_estime_eur": cout_estime_eur(etat, &modele, systeme, message, max_jetons),
            "plafond": appel::etat_plafond(&etat.backend),
        }))
        .into_response());
    }
    // Pas de cache ici : une réponse refusée par le validateur resterait
    // servie à chaque nouvel essai. Ces appels sont courts (Haiku 5.5).
    let res = appel::appeler(etat, usage, false, systeme, message, max_jetons, None)
        .await
        .map_err(|e| appel::reponse_erreur(&e))?;
    let brut = appel::objet_json(&res.texte).ok_or_else(|| {
        erreur(
            StatusCode::UNPROCESSABLE_ENTITY,
            "reponse_invalide",
            "Le modèle n'a pas rendu un objet JSON.",
        )
    })?;
    Ok((brut, res))
}

fn hors_schema(usage: &str, code: &'static str) -> Response {
    warn!(usage, code, "assistant_ia_collection_hors_schema");
    erreur(
        StatusCode::UNPROCESSABLE_ENTITY,
        code,
        "La réponse du modèle ne suit pas le schéma demandé : rien n'est proposé.",
    )
}

fn avec_cout(mut v: Value, etat: &Etat, res: &appel::Resultat) -> Response {
    v["ok"] = json!(true);
    v["depuis_cache"] = json!(res.depuis_cache);
    v["cout_eur"] = json!(couts::micro_en_euros(res.cout_micro_eur));
    v["plafond"] = appel::etat_plafond(&etat.backend);
    Json(v).into_response()
}

/// (1) Affiner en dialoguant une collection ou une playlist existante.
async fn collections_affiner(
    State(etat): State<Etat>,
    entetes: HeaderMap,
    Json(d): Json<DemandeCollection>,
) -> Response {
    let phrase = cache::normaliser_phrase(d.phrase.as_deref().unwrap_or(""));
    if phrase.is_empty() || phrase.chars().count() > 500 {
        return erreur(
            StatusCode::BAD_REQUEST,
            "phrase_invalide",
            "Dites ce qu'il faut changer en une phrase (500 caractères au plus).",
        );
    }
    let Some(actuelle) = d.actuelle.as_ref().filter(|a| a.is_object()) else {
        return erreur(
            StatusCode::BAD_REQUEST,
            "actuelle_requise",
            "Les règles actuelles sont requises.",
        );
    };
    let avant: Vec<Value> = actuelle
        .get("rules")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let cible = Cible::depuis(d.cible.as_deref().or(Some("collection")));
    let langue = langue(d.langue.as_deref(), &entetes);
    let refs = Referentiel::charger(&etat.backend);
    let offerts = regles::criteres_offerts(&etat.criteres, cible, &refs);
    let genres = genres_de_la_bibliotheque(&etat);
    let systeme = crate::collections::consigne_affiner(&regles::consigne(
        &offerts, cible, &genres, &refs, &langue,
    ));
    let lisibles = crate::collections::regles_lisibles(&avant, &offerts, &refs);
    let message = json!({
        "current": {
            "name": actuelle.get("name"),
            "match_mode": actuelle.get("match_mode"),
            "rules": lisibles,
        },
        "request": d.phrase.as_deref().unwrap_or("").trim(),
    })
    .to_string();
    let (brut, res) = match appel_collection(
        &etat,
        crate::collections::USAGE_AFFINER,
        &systeme,
        &message,
        4096,
        d.apercu,
    )
    .await
    {
        Ok(x) => x,
        Err(r) => return r,
    };
    let p = match regles::valider(&brut, &offerts, cible, &refs) {
        Ok(p) => p,
        Err(code) => return hors_schema(crate::collections::USAGE_AFFINER, code),
    };
    if p.rules.is_empty() {
        // #6731 : une liste sans règle rendrait toute la bibliothèque.
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({
                "ok": false,
                "error": "aucune_regle",
                "message": "Le modèle n'a laissé aucune règle utilisable : rien n'est proposé.",
                "refusees": p.refusees,
            })),
        )
            .into_response();
    }
    let ecart = crate::collections::ecart(&avant, &p.rules);
    info!(
        cible = cible.nom(),
        ajoutees = ecart.ajoutees.len(),
        retirees = ecart.retirees.len(),
        "assistant_ia_collection_affinee"
    );
    avec_cout(
        json!({ "cible": cible.nom(), "proposition": p, "ecart": ecart, "avant": avant }),
        &etat,
        &res,
    )
}

/// (2) Proposer 5 à 10 collections depuis les seules statistiques.
async fn collections_proposer(
    State(etat): State<Etat>,
    entetes: HeaderMap,
    corps: Option<Json<DemandeCollection>>,
) -> Response {
    let d = corps.map(|Json(d)| d).unwrap_or_default();
    let langue = langue(d.langue.as_deref(), &entetes);
    let refs = Referentiel::charger(&etat.backend);
    let offerts = regles::criteres_offerts(&etat.criteres, Cible::Collection, &refs);
    let systeme = crate::collections::consigne_proposer(
        &regles::consigne(&offerts, Cible::Collection, &[], &refs, &langue),
        &langue,
    );
    let message = crate::collections::statistiques(&etat.backend, &refs).to_string();
    propositions(
        &etat,
        crate::collections::USAGE_PROPOSER,
        &systeme,
        &message,
        &offerts,
        &refs,
        d.apercu,
    )
    .await
}

async fn propositions(
    etat: &Etat,
    usage: &str,
    systeme: &str,
    message: &str,
    offerts: &[regles::Offert],
    refs: &Referentiel,
    apercu: bool,
) -> Response {
    let (brut, res) = match appel_collection(etat, usage, systeme, message, 8192, apercu).await {
        Ok(x) => x,
        Err(r) => return r,
    };
    match crate::collections::valider_propositions(&brut, offerts, refs) {
        Ok((collections, ecartees)) => {
            info!(
                usage,
                retenues = collections.len(),
                ecartees = ecartees.len(),
                "assistant_ia_collections_proposees"
            );
            avec_cout(
                json!({ "collections": collections, "ecartees": ecartees }),
                etat,
                &res,
            )
        }
        Err(code) => hors_schema(usage, code),
    }
}

/// (3) Nommer et décrire.
async fn collections_nommer(
    State(etat): State<Etat>,
    entetes: HeaderMap,
    Json(d): Json<DemandeCollection>,
) -> Response {
    let rules = d.rules.clone().unwrap_or_default();
    if rules.is_empty() {
        return erreur(
            StatusCode::BAD_REQUEST,
            "regles_requises",
            "Les règles sont requises.",
        );
    }
    let cible = Cible::depuis(d.cible.as_deref().or(Some("collection")));
    let langue = langue(d.langue.as_deref(), &entetes);
    let refs = Referentiel::charger(&etat.backend);
    let offerts = regles::criteres_offerts(&etat.criteres, cible, &refs);
    let systeme = crate::collections::consigne_nommer(cible, &langue);
    let message = json!({
        "match_mode": d.match_mode.as_deref().unwrap_or("all"),
        "rules": crate::collections::regles_lisibles(&rules, &offerts, &refs),
    })
    .to_string();
    let (brut, res) = match appel_collection(
        &etat,
        crate::collections::USAGE_NOMMER,
        &systeme,
        &message,
        1024,
        d.apercu,
    )
    .await
    {
        Ok(x) => x,
        Err(r) => return r,
    };
    match crate::collections::valider_nom(&brut) {
        Ok(v) => avec_cout(v, &etat, &res),
        Err(code) => hors_schema(crate::collections::USAGE_NOMMER, code),
    }
}

/// (4) Pourquoi cet album est dans la collection.
async fn collections_pourquoi(
    State(etat): State<Etat>,
    entetes: HeaderMap,
    Json(d): Json<DemandeCollection>,
) -> Response {
    let rules = d.rules.clone().unwrap_or_default();
    let Some(album_id) = d.album_id else {
        return erreur(
            StatusCode::BAD_REQUEST,
            "album_requis",
            "L'album est requis.",
        );
    };
    if rules.is_empty() {
        return erreur(
            StatusCode::BAD_REQUEST,
            "regles_requises",
            "Les règles sont requises.",
        );
    }
    let champs = match crate::collections::champs_album(&etat.backend, album_id) {
        Ok(Some(c)) => c,
        Ok(None) => {
            return erreur(
                StatusCode::NOT_FOUND,
                "album_introuvable",
                "Album introuvable.",
            );
        }
        Err(e) => return interne(e),
    };
    let langue = langue(d.langue.as_deref(), &entetes);
    let refs = Referentiel::charger(&etat.backend);
    let offerts = regles::criteres_offerts(&etat.criteres, Cible::Collection, &refs);
    let lisibles = crate::collections::regles_lisibles(&rules, &offerts, &refs);
    let numerotees: Vec<Value> = lisibles
        .iter()
        .enumerate()
        .map(|(i, r)| json!({ "index": i, "rule": r }))
        .collect();
    let systeme = crate::collections::consigne_pourquoi(&langue);
    let message = json!({
        "match_mode": d.match_mode.as_deref().unwrap_or("all"),
        "rules": numerotees,
        "album": champs,
    })
    .to_string();
    let (brut, res) = match appel_collection(
        &etat,
        crate::collections::USAGE_POURQUOI,
        &systeme,
        &message,
        1024,
        d.apercu,
    )
    .await
    {
        Ok(x) => x,
        Err(r) => return r,
    };
    match crate::collections::valider_pourquoi(&brut, rules.len()) {
        Ok(v) => avec_cout(v, &etat, &res),
        Err(code) => hors_schema(crate::collections::USAGE_POURQUOI, code),
    }
}

/// (5) Collections d'après l'écoute — Premium, comme « Mon écoute ».
async fn collections_ecoute(
    State(etat): State<Etat>,
    entetes: HeaderMap,
    corps: Option<Json<DemandeCollection>>,
) -> Response {
    if !etat.ecoute_premium().await {
        return erreur(
            StatusCode::PAYMENT_REQUIRED,
            "premium_requis",
            "Les collections d'après l'écoute font partie de Premium, comme « Mon écoute ».",
        );
    }
    let d = corps.map(|Json(d)| d).unwrap_or_default();
    let langue = langue(d.langue.as_deref(), &entetes);
    let refs = Referentiel::charger(&etat.backend);
    let offerts = regles::criteres_offerts(&etat.criteres, Cible::Collection, &refs);
    let systeme = crate::collections::consigne_ecoute(
        &regles::consigne(&offerts, Cible::Collection, &[], &refs, &langue),
        &langue,
    );
    let message = crate::collections::agregats_ecoute(&etat.backend, d.profil_id).to_string();
    propositions(
        &etat,
        crate::collections::USAGE_ECOUTE,
        &systeme,
        &message,
        &offerts,
        &refs,
        d.apercu,
    )
    .await
}

// ---------------------------------------------------------------------------
// Diagnostic en lecture seule (23/10/2026)
// ---------------------------------------------------------------------------

#[derive(Deserialize, Default)]
struct DemandeDiagnostic {
    zone_id: Option<i64>,
    question: Option<String>,
    langue: Option<String>,
    #[serde(default)]
    second_avis: bool,
}

/// L'aperçu : ce qui partirait, EXACTEMENT, et son coût estimé. Rien ne part.
async fn diagnostic_apercu(
    State(etat): State<Etat>,
    entetes: HeaderMap,
    Json(d): Json<DemandeDiagnostic>,
) -> Response {
    let Some(collecte) = etat.collecte_diagnostic.clone() else {
        return erreur(
            StatusCode::SERVICE_UNAVAILABLE,
            "diagnostic_indisponible",
            "Le diagnostic n'est pas disponible sur ce serveur.",
        );
    };
    let question = d.question.as_deref().unwrap_or("").trim().to_string();
    if question.chars().count() > crate::diagnostic::QUESTION_MAX {
        return erreur(
            StatusCode::BAD_REQUEST,
            "question_invalide",
            "La question fait 500 caractères au plus.",
        );
    }
    let brut = match collecte(d.zone_id).await {
        Ok(b) => b,
        Err(e) => return interne(e),
    };
    let langue = langue(d.langue.as_deref(), &entetes);
    let p = crate::diagnostic::preparer(&brut, &question, &langue);
    let choix = if d.second_avis {
        appel::Choix::Fort
    } else {
        appel::Choix::Defaut
    };
    let r = reglages::Reglages::charger(&etat.backend);
    let modele = r.modele_choisi(choix);
    let corps = json!({
        "ok": true,
        "fournisseur": r.famille.map(crate::fournisseur::Famille::nom),
        "modele": modele,
        // Ollama : rien ne quitte le réseau de l'utilisateur.
        "local": r.famille == Some(crate::fournisseur::Famille::Ollama),
        "envoye": { "systeme": p.systeme, "message": p.message },
        "lignes_journal": p.lignes_journal,
        "remplacements": p.pseudonymes.taille(),
        "cout_estime_eur": cout_estime_eur(&etat, &modele, &p.systeme, &p.message, crate::diagnostic::MAX_JETONS),
        "plafond": appel::etat_plafond(&etat.backend),
        "duree_validite_s": crate::diagnostic::APERCU_DUREE.as_secs(),
    });
    let id = etat.apercus.ranger(p, choix);
    let mut corps = corps;
    corps["apercu_id"] = json!(id);
    info!(zone = d.zone_id.is_some(), "assistant_ia_diagnostic_apercu");
    Json(corps).into_response()
}

#[derive(Deserialize)]
struct EnvoiDiagnostic {
    apercu_id: String,
}

/// L'envoi d'un aperçu MONTRÉ, une seule fois. Aucune action : la réponse
/// est un constat et des gestes à essayer, que l'utilisateur fait lui-même.
async fn diagnostic_envoyer(State(etat): State<Etat>, Json(d): Json<EnvoiDiagnostic>) -> Response {
    let Some(a) = etat.apercus.prendre(d.apercu_id.trim()) else {
        return erreur(
            StatusCode::CONFLICT,
            "apercu_requis",
            "Montrez d'abord ce qui sera envoyé : cet aperçu n'existe pas, a déjà servi ou a expiré.",
        );
    };
    let res = match appel::appeler_choix(
        &appel::Moyens::de(&etat),
        crate::diagnostic::USAGE_DIAGNOSTIC,
        a.choix,
        &a.systeme,
        &a.message,
        crate::diagnostic::MAX_JETONS,
        None,
        true,
    )
    .await
    {
        Ok(r) => r,
        Err(e) => return appel::reponse_erreur(&e),
    };
    let Some(brut) = appel::objet_json(&res.texte) else {
        return erreur(
            StatusCode::UNPROCESSABLE_ENTITY,
            "reponse_invalide",
            "Le modèle n'a pas rendu un objet JSON.",
        );
    };
    let diag = match crate::diagnostic::valider(&brut, &a.message) {
        Ok(x) => x,
        Err(code) => {
            warn!(code, "assistant_ia_diagnostic_hors_schema");
            return erreur(
                StatusCode::UNPROCESSABLE_ENTITY,
                code,
                "La réponse du modèle ne suit pas le schéma demandé.",
            );
        }
    };
    info!(
        causes = diag.causes.len(),
        ecartes = diag.ecartes.len(),
        "assistant_ia_diagnostic"
    );
    let mut v = crate::diagnostic::pour_l_ecran(&diag, &a.pseudonymes);
    v["ok"] = json!(true);
    v["cout_eur"] = json!(couts::micro_en_euros(res.cout_micro_eur));
    v["plafond"] = appel::etat_plafond(&etat.backend);
    Json(v).into_response()
}
