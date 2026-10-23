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
        .route(
            "/albums/{id}/notes",
            post(notes_ecrire).get(notes_lire).delete(notes_effacer),
        )
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
// Notes de pochette (23/10/2026)
// ---------------------------------------------------------------------------

#[derive(Deserialize, Default)]
struct DemandeNotes {
    langue: Option<String>,
    /// Le second avis (Sonnet 5.5 chez Anthropic), demandé explicitement.
    #[serde(default)]
    second_avis: bool,
    /// Ne pas servir la réponse en cache : un nouvel appel, payé.
    #[serde(default)]
    regenerer: bool,
    /// Rien n'est envoyé : rend ce qui PARTIRAIT et le coût estimé.
    #[serde(default)]
    apercu: bool,
}

async fn notes_ecrire(
    State(etat): State<Etat>,
    Path(id): Path<i64>,
    entetes: HeaderMap,
    corps: Option<Json<DemandeNotes>>,
) -> Response {
    let d = corps.map(|Json(d)| d).unwrap_or_default();
    let ctx = match crate::notes::charger(&etat.backend, id) {
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
    let sources = ctx.sources();
    let systeme = crate::notes::consigne(&langue, &sources);
    let message = ctx.message();
    let choix = if d.second_avis {
        appel::Choix::Fort
    } else {
        appel::Choix::Defaut
    };
    let modele = reglages::Reglages::charger(&etat.backend).modele_choisi(choix);
    if d.apercu {
        return Json(json!({
            "ok": true,
            "apercu": true,
            "modele": modele,
            "envoye": { "systeme": systeme, "message": message },
            "cout_estime_eur": cout_estime_eur(&etat, &modele, &systeme, &message, crate::notes::MAX_JETONS),
            "plafond": appel::etat_plafond(&etat.backend),
        }))
        .into_response();
    }
    let entree = format!("{systeme}\n{message}");
    let res = match appel::appeler_choix(
        &appel::Moyens::de(&etat),
        crate::notes::USAGE_NOTES,
        choix,
        &systeme,
        &message,
        crate::notes::MAX_JETONS,
        (!d.regenerer).then_some(entree.as_str()),
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
    let notes = match crate::notes::valider(&brut, &ctx) {
        Ok(n) => n,
        Err(code) => {
            warn!(album_id = id, code, "assistant_ia_notes_hors_schema");
            return erreur(
                StatusCode::UNPROCESSABLE_ENTITY,
                code,
                "La réponse du modèle ne suit pas le schéma demandé (chaque paragraphe doit citer ses sources).",
            );
        }
    };
    info!(
        album_id = id,
        paragraphes = notes.paragraphes.len(),
        ecartes = notes.ecartes.len(),
        "assistant_ia_notes"
    );
    let rangees = json!({
        "album_id": id,
        "marque": "ia",
        "bandeau": "Écrit par l'IA à partir de vos métadonnées",
        "modele": modele,
        "langue": langue,
        "cree_le": chrono::Utc::now().to_rfc3339(),
        "notes": notes,
        "attributions": ctx.attributions,
    });
    etat.cache
        .ecrire(&crate::notes::cle_rangement(id), &rangees.to_string());
    let mut v = rangees;
    v["ok"] = json!(true);
    v["depuis_cache"] = json!(res.depuis_cache);
    v["cout_eur"] = json!(couts::micro_en_euros(res.cout_micro_eur));
    v["plafond"] = appel::etat_plafond(&etat.backend);
    Json(v).into_response()
}

async fn notes_lire(State(etat): State<Etat>, Path(id): Path<i64>) -> Response {
    let rangees = etat
        .cache
        .lire(&crate::notes::cle_rangement(id))
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .filter(|_| crate::notes::album_existe(&etat.backend, id));
    match rangees {
        Some(mut v) => {
            v["ok"] = json!(true);
            Json(v).into_response()
        }
        None => erreur(
            StatusCode::NOT_FOUND,
            "aucune_note",
            "Aucune note pour cet album.",
        ),
    }
}

async fn notes_effacer(State(etat): State<Etat>, Path(id): Path<i64>) -> Response {
    etat.cache.effacer(&crate::notes::cle_rangement(id));
    Json(json!({ "ok": true })).into_response()
}
