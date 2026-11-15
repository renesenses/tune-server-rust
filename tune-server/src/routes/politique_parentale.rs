//! Contrôle parental, étape 1 (#7856) : routes de la politique d'un profil et
//! filtrage des listes servies à un profil supervisé.
//!
//! `GET|PUT|DELETE /profiles/{id}/politique-parentale`, réservées au
//! superviseur de la politique et aux administrateurs ; le profil supervisé
//! n'y a aucun droit (403). Une écriture porte la révision lue : une révision
//! périmée est refusée (409). La règle d'accès est
//! [`tune_core::db::politique_parentale::peut_gerer`].
//!
//! Un profil administrateur ne se place pas sous supervision : poser sa
//! politique est refusé (422, `profil_administrateur_non_supervisable`).
//!
//! Le filtrage est une couche posée sur l'API : pour les seules routes de
//! listes (bibliothèque, recherche, radios, podcasts) et un profil qui a une
//! politique, la réponse est relue et les éléments bloqués en sont retirés.
//! Chaque GET d'une de ces routes coûte une lecture de la politique du profil
//! (une requête par clé primaire) ; sans politique, le corps de la réponse
//! passe tel quel, sans être relu. Si la table manque (création échouée au
//! démarrage), aucune politique ne peut exister : la liste passe, et l'écart
//! est journalisé. Une politique présente mais illisible refuse la liste ;
//! sa lecture rend 500, mais son superviseur peut encore la retirer (DELETE)
//! ou la remplacer (PUT), la règle d'accès ne lisant pas son contenu.
//!
//! #8088 — « Masquer les contenus explicites » (`masquer_explicite`) : sur
//! les routes de contenus (bibliothèque, recherche, pages des services,
//! playlists, voir [`route_a_contenus`]), les pistes et albums marqués
//! explicites sont retirés à toute profondeur — marquage du service
//! (`explicit`, `parental_warning`) ou de la balise du fichier
//! (`tracks.explicit`, lu en base une fois par requête). La fiche d'un album
//! ou d'une piste explicite est refusée (403, `parental_explicit_refused`).
//! Sans la case, ces routes ne relisent rien de plus qu'avant.
//!
//! #8611 — un artiste bloqué l'est partout : playlists, historique
//! (`/history`, `/profiles/{id}/history`), favoris de la bibliothèque
//! (`/profiles/{id}/favorites`, résolus par lectures ciblées en base),
//! favoris et résultats des services (`/streaming/*`,
//! `/profiles/{id}/favorites/streaming`, par le NOM seul : un identifiant
//! de service n'est pas un artiste de la bibliothèque). Le flux d'une piste
//! demandé par son identifiant (`/library/tracks/{id}/audio`, adresse du flux
//! d'un service) est refusé (403 `parental_content_blocked`) quand l'appelant
//! se nomme (`X-Profile-Id` ou session) : un renderer qui tire le flux sans
//! se nommer joue ce que l'orchestrateur, gardé lui aussi, lui a envoyé.
//!
//! Le profil appelant est celui d'[`ActiveProfile`] : avec l'authentification
//! active, celui du compte connecté.

use axum::Json;
use axum::body::Body;
use axum::extract::{FromRequestParts, Path, Query, Request, State};
use axum::http::{Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::{Value, json};
use tune_core::db::politique_parentale::{
    self as pp, Ecriture, Fiche, Filtre, Liste, PolitiqueParentale,
};

use crate::routes::active_profile::ActiveProfile;
use crate::state::AppState;

/// Plafond de la réponse relue pour être filtrée.
const CORPS_MAX: usize = 64 * 1024 * 1024;

fn refus(statut: StatusCode, code: &str) -> Response {
    (statut, Json(json!({ "error": code }))).into_response()
}

fn interne(e: String) -> Response {
    tracing::warn!(error = %e, "politique_parentale_erreur");
    refus(
        StatusCode::INTERNAL_SERVER_ERROR,
        "politique_parentale_indisponible",
    )
}

/// Le profil visé existe, et l'appelant peut gérer sa politique.
fn controler(state: &AppState, appelant: i64, cible: i64) -> Result<(), Response> {
    let existe = tune_core::db::profile_repo::ProfileRepo::with_backend(state.backend.clone())
        .get(cible)
        .map_err(interne)?
        .is_some();
    if !existe {
        return Err(refus(StatusCode::NOT_FOUND, "profil_introuvable"));
    }
    match pp::peut_gerer(&state.backend, appelant, cible) {
        Ok(true) => Ok(()),
        Ok(false) => Err(refus(
            StatusCode::FORBIDDEN,
            "politique_parentale_reservee_au_superviseur",
        )),
        Err(e) => Err(interne(e)),
    }
}

fn corps(p: &pp::PolitiqueEnregistree) -> Value {
    json!({
        "profile_id": p.profile_id,
        "superviseur_id": p.superviseur_id,
        "revision": p.revision,
        "politique": p.politique,
        "maj_le": p.maj_le,
    })
}

pub(crate) async fn lire(
    State(state): State<AppState>,
    ActiveProfile(appelant): ActiveProfile,
    Path(cible): Path<i64>,
) -> Response {
    if let Err(r) = controler(&state, appelant, cible) {
        return r;
    }
    match pp::lire(&state.backend, cible) {
        Ok(Some(p)) => Json(corps(&p)).into_response(),
        Ok(None) => refus(StatusCode::NOT_FOUND, "politique_parentale_absente"),
        Err(e) => interne(e),
    }
}

#[derive(Deserialize)]
pub(crate) struct Ecrire {
    /// 0 pour une création, la révision lue sinon.
    revision: i64,
    politique: PolitiqueParentale,
}

pub(crate) async fn poser(
    State(state): State<AppState>,
    ActiveProfile(appelant): ActiveProfile,
    Path(cible): Path<i64>,
    Json(demande): Json<Ecrire>,
) -> Response {
    if let Err(r) = controler(&state, appelant, cible) {
        return r;
    }
    match pp::est_administrateur(&state.backend, cible) {
        Ok(false) => {}
        Ok(true) => {
            return refus(
                StatusCode::UNPROCESSABLE_ENTITY,
                "profil_administrateur_non_supervisable",
            );
        }
        Err(e) => return interne(e),
    }
    if let Err(code) = demande.politique.valider() {
        return refus(StatusCode::UNPROCESSABLE_ENTITY, code);
    }
    match pp::poser(
        &state.backend,
        cible,
        appelant,
        demande.revision,
        &demande.politique,
    ) {
        Ok(Ecriture::Faite(_)) => match pp::lire(&state.backend, cible) {
            Ok(Some(p)) => Json(corps(&p)).into_response(),
            Ok(None) => refus(StatusCode::CONFLICT, "revision_perimee"),
            Err(e) => interne(e),
        },
        Ok(Ecriture::Conflit) => refus(StatusCode::CONFLICT, "revision_perimee"),
        Err(e) => interne(e),
    }
}

#[derive(Deserialize)]
pub(crate) struct Retrait {
    revision: i64,
}

pub(crate) async fn retirer(
    State(state): State<AppState>,
    ActiveProfile(appelant): ActiveProfile,
    Path(cible): Path<i64>,
    Query(q): Query<Retrait>,
) -> Response {
    if let Err(r) = controler(&state, appelant, cible) {
        return r;
    }
    match pp::retirer(&state.backend, cible, q.revision) {
        Ok(Ecriture::Faite(_)) => StatusCode::NO_CONTENT.into_response(),
        Ok(Ecriture::Conflit) => refus(StatusCode::CONFLICT, "revision_perimee"),
        Err(e) => interne(e),
    }
}

/// Les routes de listes filtrées, et ce que leurs éléments sont.
fn liste_de_la_route(chemin: &str) -> Option<Liste> {
    let chemin = chemin.strip_prefix("/api/v1").unwrap_or(chemin);
    let chemin = chemin.trim_end_matches('/');
    match chemin {
        "/library/albums" | "/library/tracks" | "/library/search" | "/search" => {
            Some(Liste::Bibliotheque)
        }
        "/library/artists" => Some(Liste::Artistes),
        "/radios" | "/radios/search" | "/radios/favorites" | "/radios/directory" => {
            Some(Liste::Radios)
        }
        "/podcasts/subscriptions"
        | "/podcasts/search"
        | "/podcasts/discover"
        | "/podcasts/top"
        | "/podcasts/catalogue" => Some(Liste::Podcasts),
        // #8611 — l'historique, les playlists (pistes, fiche, partagées,
        // collaboratives) et l'historique d'un profil.
        c if c == "/history" || c.starts_with("/history/") => Some(Liste::Bibliotheque),
        c if c == "/playlists" || c.starts_with("/playlists/") => Some(Liste::Bibliotheque),
        c if c.starts_with("/profiles/") && c.ends_with("/history") => Some(Liste::Bibliotheque),
        // #8611 — les services : résultats, fiches, favoris ; par le nom.
        c if c.starts_with("/streaming/") => Some(Liste::Services),
        c if c.starts_with("/profiles/") && c.ends_with("/favorites/streaming") => {
            Some(Liste::Services)
        }
        // #7913 — les rangées de l'accueil, les genres, les collections, les
        // listes intelligentes et les sous-listes de la bibliothèque (albums
        // d'un artiste, pistes d'un album…) : un artiste bloqué n'y paraît
        // plus. Les fiches elles-mêmes sont refusées (`fiche_de_la_route`).
        c if c == "/home" || c.starts_with("/home/") || c.starts_with("/library/") => {
            Some(Liste::Bibliotheque)
        }
        _ => None,
    }
}

/// #7913 — les fiches ouvertes directement, refusées (403) quand leur objet
/// est bloqué pour le profil.
fn fiche_de_la_route(chemin: &str) -> Option<Fiche> {
    let chemin = chemin.strip_prefix("/api/v1").unwrap_or(chemin);
    let segments: Vec<&str> = chemin.trim_matches('/').split('/').collect();
    let nombre = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    match segments.as_slice() {
        ["library", "albums", id] if nombre(id) => Some(Fiche::Album),
        ["library", "artists", id] if nombre(id) => Some(Fiche::Artiste),
        ["library", "tracks", id] if nombre(id) => Some(Fiche::Piste),
        ["radios", id] if nombre(id) => Some(Fiche::Radio),
        ["podcasts", "episodes"] | ["podcasts", "episodes", _] => Some(Fiche::Podcast),
        _ => None,
    }
}

/// #7913 — le code du refus d'une fiche bloquée ; #8611 — et d'une lecture.
pub(crate) const CODE_REFUS_FICHE_BLOQUEE: &str = pp::CODE_REFUS_CONTENU_BLOQUE;

/// #8611 — le corps du refus d'un contenu dont l'artiste est bloqué : code
/// stable et phrase traduite.
pub(crate) fn corps_du_refus_contenu_bloque(lang: &str) -> Value {
    json!({
        "error": CODE_REFUS_FICHE_BLOQUEE,
        "code": CODE_REFUS_FICHE_BLOQUEE,
        "message": crate::i18n::t(lang, "parental.content.blocked"),
    })
}

/// #8611 — le refus d'un contenu dont l'artiste est bloqué : 403.
pub(crate) fn refus_contenu_bloque(lang: &str) -> Response {
    (
        StatusCode::FORBIDDEN,
        Json(corps_du_refus_contenu_bloque(lang)),
    )
        .into_response()
}

/// #8611 — la fiche d'un album, d'un artiste ou d'une piste d'un service
/// (`/streaming/{service}/albums|artists|tracks/{id}`) : `Some(true)` pour
/// un artiste.
fn fiche_de_service(uri: &axum::http::Uri) -> Option<bool> {
    let chemin = uri.path();
    let chemin = chemin.strip_prefix("/api/v1").unwrap_or(chemin);
    let segments: Vec<&str> = chemin.trim_matches('/').split('/').collect();
    match segments.as_slice() {
        ["streaming", _, "artists", _] => Some(true),
        ["streaming", _, "albums" | "tracks", _] => Some(false),
        _ => None,
    }
}

/// #8611 — la liste des favoris de la bibliothèque d'un profil
/// (`/profiles/{id}/favorites`) : des couples (`item_type`, `item_id`) dont
/// l'artiste se lit en base.
fn favoris_de_la_route(chemin: &str) -> bool {
    let chemin = chemin.strip_prefix("/api/v1").unwrap_or(chemin);
    let segments: Vec<&str> = chemin.trim_matches('/').split('/').collect();
    matches!(segments.as_slice(), ["profiles", _, "favorites"])
}

/// #8611 — les routes qui servent le flux d'UNE piste désignée par son
/// identifiant : la demande de lecture qu'elles valent.
fn flux_de_la_route(chemin: &str) -> Option<tune_core::orchestrator::PlayRequest> {
    let chemin = chemin.strip_prefix("/api/v1").unwrap_or(chemin);
    let segments: Vec<&str> = chemin.trim_matches('/').split('/').collect();
    let demande = |source: &str, id: &str| tune_core::orchestrator::PlayRequest {
        source: Some(source.to_string()),
        source_id: Some(id.to_string()),
        ..Default::default()
    };
    match segments.as_slice() {
        ["library", "tracks", id, "audio"] => {
            id.parse::<i64>()
                .ok()
                .map(|id| tune_core::orchestrator::PlayRequest {
                    track_id: Some(id),
                    ..Default::default()
                })
        }
        ["streaming", service, "tracks", id, "url"] => Some(demande(service, id)),
        ["soundcloud", "tracks", id, "stream"] => Some(demande("soundcloud", id)),
        _ => None,
    }
}

/// #8611 — le profil que l'appelant NOMME (`X-Profile-Id`, ou session
/// authentifiée), `None` s'il ne se nomme pas (renderer, serveur média).
async fn profil_nomme(parts: &mut axum::http::request::Parts, state: &AppState) -> Option<i64> {
    let nomme = parts.headers.contains_key("X-Profile-Id")
        || parts.extensions.get::<crate::auth::AuthUser>().is_some();
    if !nomme {
        return None;
    }
    match ActiveProfile::from_request_parts(parts, state).await {
        Ok(ActiveProfile(p)) => Some(p),
        Err(never) => match never {},
    }
}

/// #8611 — garde du flux d'une piste par son identifiant. `Some(refus)` si
/// l'artiste de la piste est bloqué pour le profil nommé.
async fn garder_le_flux(
    state: &AppState,
    parts: &mut axum::http::request::Parts,
    demande: &tune_core::orchestrator::PlayRequest,
) -> Option<Response> {
    let profil = profil_nomme(parts, state).await?;
    let politique = match pp::lire(&state.backend, profil) {
        Ok(Some(p)) => p.politique,
        Ok(None) => return None,
        Err(e) if pp::table_absente(&e) => return None,
        Err(e) => return Some(interne(e)),
    };
    let filtre = Filtre::depuis(&politique);
    if !state
        .orchestrator
        .demande_d_un_artiste_bloque(&filtre, demande)
        .await
    {
        return None;
    }
    tracing::info!(
        profil,
        track_id = ?demande.track_id,
        source = ?demande.source,
        source_id = ?demande.source_id,
        "parental_content_blocked_stream"
    );
    let lang = crate::i18n::lang_from_header(&parts.headers);
    Some(refus_contenu_bloque(&lang))
}

/// #7913 — marque posée sur les requêtes INTERNES de recomptage : les
/// handlers de listes n'y font pas leurs annotations (écoutes de la
/// communauté, un appel HTTP au site par page).
#[derive(Clone, Copy, Debug)]
pub(crate) struct RecomptageParental;

/// Durée de vie d'un décompte mis en cache.
const DUREE_DU_DECOMPTE: std::time::Duration = std::time::Duration::from_secs(600);

/// Un décompte d'éléments bloqués, valable tant que la politique (révision),
/// la liste (total servi) et la bibliothèque (empreinte) n'ont pas changé.
#[derive(Clone)]
struct Decompte {
    total_servi: u64,
    empreinte: String,
    bloques: u64,
    quand: std::time::Instant,
}

type CleDuDecompte = (i64, String, i64);

fn decomptes() -> &'static std::sync::Mutex<std::collections::HashMap<CleDuDecompte, Decompte>> {
    static D: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<CleDuDecompte, Decompte>>,
    > = std::sync::OnceLock::new();
    D.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// Nombre de pages relues pour un recomptage (témoins et mesure).
pub static PAGES_RELUES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// L'empreinte de la bibliothèque : nombre et plus grand identifiant des
/// albums, pistes et artistes. Un scan, un ajout ou un retrait la changent ;
/// une modification en place (renommage) attend la fin de `DUREE_DU_DECOMPTE`.
fn empreinte_de_la_bibliotheque(state: &AppState) -> String {
    let mut e = String::new();
    for table in ["albums", "tracks", "artists"] {
        let r = state
            .backend
            .query_one(
                &format!("SELECT COUNT(*), COALESCE(MAX(id), 0) FROM {table}"),
                &[],
            )
            .ok()
            .flatten()
            .map(|r| {
                format!(
                    "{}:{}",
                    r.first().and_then(|v| v.as_i64()).unwrap_or(-1),
                    r.get(1).and_then(|v| v.as_i64()).unwrap_or(-1)
                )
            })
            .unwrap_or_default();
        e.push_str(&r);
        e.push('|');
    }
    e
}

/// La liste sans sa fenêtre : la clé du décompte.
fn chemin_sans_fenetre(uri: &axum::http::Uri) -> String {
    let mut q: Vec<&str> = uri
        .query()
        .unwrap_or("")
        .split('&')
        .filter(|p| !p.is_empty())
        .filter(|p| !matches!(p.split('=').next().unwrap_or(""), "limit" | "offset"))
        .collect();
    q.sort_unstable();
    format!("{}?{}", uri.path(), q.join("&"))
}

/// Taille des pages relues pour compter le total exact.
const PAGE_DE_COMPTAGE: u64 = 500;
/// Plafond de pages relues : au-delà, le total reste celui de la page.
const PAGES_DE_COMPTAGE_MAX: usize = 400;

/// [`bloques_dans_toute_la_liste`], gardé en cache par (profil, liste sans
/// fenêtre, révision de la politique) : la liste n'est relue que si son total,
/// la bibliothèque ou la politique ont changé, ou après `DUREE_DU_DECOMPTE`.
/// Feuilleter une liste ne la relit donc qu'une fois.
#[allow(clippy::too_many_arguments)]
async fn bloques_avec_cache(
    state: &AppState,
    cle: CleDuDecompte,
    suite: &Next,
    gabarit: &(
        axum::http::Uri,
        axum::http::HeaderMap,
        axum::http::Extensions,
    ),
    graine: Option<&Value>,
    filtre: &Filtre,
    liste: Liste,
    total: u64,
) -> Option<u64> {
    let empreinte = empreinte_de_la_bibliotheque(state);
    if let Ok(d) = decomptes().lock()
        && let Some(c) = d.get(&cle)
        && c.total_servi == total
        && c.empreinte == empreinte
        && c.quand.elapsed() < DUREE_DU_DECOMPTE
    {
        return Some(c.bloques);
    }
    let debut = std::time::Instant::now();
    let bloques = bloques_dans_toute_la_liste(suite, gabarit, graine, filtre, liste, total).await?;
    tracing::debug!(
        profil = cle.0,
        liste = %cle.1,
        total,
        bloques,
        ms = debut.elapsed().as_millis() as u64,
        "politique_parentale_total_recompte"
    );
    if let Ok(mut d) = decomptes().lock() {
        d.retain(|_, c| c.quand.elapsed() < DUREE_DU_DECOMPTE);
        d.insert(
            cle,
            Decompte {
                total_servi: total,
                empreinte,
                bloques,
                quand: std::time::Instant::now(),
            },
        );
    }
    Some(bloques)
}

/// #7913 — le total EXACT d'une liste paginée pour un profil supervisé.
///
/// Le filtre ne voit que la page servie : un `total` diminué des seuls
/// éléments retirés de CETTE page comptait encore ceux des autres pages. La
/// liste est donc relue page à page (même requête, `offset`/`limit`
/// réécrits, la graine d'un tri aléatoire reprise), et ses éléments bloqués
/// comptés. Rend `None` si une relecture échoue : le total reste alors celui
/// de la page, comme avant.
async fn bloques_dans_toute_la_liste(
    suite: &Next,
    gabarit: &(
        axum::http::Uri,
        axum::http::HeaderMap,
        axum::http::Extensions,
    ),
    graine: Option<&Value>,
    filtre: &Filtre,
    liste: Liste,
    total: u64,
) -> Option<u64> {
    let (uri, entetes, extensions) = gabarit;
    // Les paramètres de la requête, gardés tels quels (encodés), sauf la
    // fenêtre et la graine, réécrites à chaque page.
    let garder: Vec<&str> = uri
        .query()
        .unwrap_or("")
        .split('&')
        .filter(|p| !p.is_empty())
        .filter(|p| {
            let cle = p.split('=').next().unwrap_or("");
            !matches!(cle, "limit" | "offset" | "seed")
        })
        .collect();
    let graine = graine.and_then(|g| match g {
        Value::Number(n) => Some(n.to_string()),
        Value::String(s) => Some(urlencoding::encode(s).into_owned()),
        _ => None,
    });
    let mut bloques = 0u64;
    let mut offset = 0u64;
    for _ in 0..PAGES_DE_COMPTAGE_MAX {
        let mut q: Vec<String> = garder.iter().map(|p| p.to_string()).collect();
        q.push(format!("limit={PAGE_DE_COMPTAGE}"));
        q.push(format!("offset={offset}"));
        if let Some(g) = &graine {
            q.push(format!("seed={g}"));
        }
        let q = q.join("&");
        let cible = format!("{}?{}", uri.path(), q);
        let mut req = axum::http::Request::builder()
            .method(Method::GET)
            .uri(cible)
            .body(Body::empty())
            .ok()?;
        *req.headers_mut() = entetes.clone();
        *req.extensions_mut() = extensions.clone();
        req.extensions_mut().insert(RecomptageParental);
        PAGES_RELUES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let reponse = suite.clone().run(req).await;
        if !reponse.status().is_success() {
            return None;
        }
        let octets = axum::body::to_bytes(reponse.into_body(), CORPS_MAX)
            .await
            .ok()?;
        let page: Value = serde_json::from_slice(&octets).ok()?;
        let n = page.get("items").and_then(Value::as_array)?.len() as u64;
        bloques += filtre.bloques_au_premier_rang(&page, liste) as u64;
        offset += n;
        if n == 0 || offset >= total {
            return Some(bloques);
        }
    }
    None
}

/// #8088 — les routes dont les contenus explicites sont retirés pour un
/// profil dont la politique les masque : bibliothèque, recherche, pages des
/// services, playlists. Plus large que [`liste_de_la_route`] : le marquage
/// explicite se lit sur l'objet lui-même, il ne dépend pas de la forme de la
/// liste.
fn route_a_contenus(chemin: &str) -> bool {
    let chemin = chemin.strip_prefix("/api/v1").unwrap_or(chemin);
    ["/library", "/search", "/streaming", "/playlists"]
        .iter()
        .any(|p| chemin == *p || chemin.strip_prefix(p).is_some_and(|r| r.starts_with('/')))
}

/// #8088 — le corps du refus d'un contenu explicite (fiche d'album ou de
/// piste, lecture) : code stable et phrase traduite.
pub(crate) fn corps_du_refus_explicite(lang: &str) -> Value {
    json!({
        "error": pp::CODE_REFUS_EXPLICITE,
        "code": pp::CODE_REFUS_EXPLICITE,
        "message": crate::i18n::t(lang, "parental.explicit.refused"),
    })
}

/// #8088 — le refus d'un contenu explicite : 403, la politique du profil
/// interdit, ce n'est pas une panne.
pub(crate) fn refus_explicite(lang: &str) -> Response {
    (StatusCode::FORBIDDEN, Json(corps_du_refus_explicite(lang))).into_response()
}

/// La couche de filtrage des listes (voir l'en-tête du module).
pub(crate) async fn filtrer_les_listes(
    State(state): State<AppState>,
    req: Request,
    next: Next,
) -> Response {
    // #8611 — le flux d'une piste par son identifiant (GET et HEAD).
    if matches!(*req.method(), Method::GET | Method::HEAD)
        && let Some(demande) = flux_de_la_route(req.uri().path())
    {
        let (mut parts, body) = req.into_parts();
        if let Some(refus) = garder_le_flux(&state, &mut parts, &demande).await {
            return refus;
        }
        return next.run(Request::from_parts(parts, body)).await;
    }
    if req.method() != Method::GET {
        return next.run(req).await;
    }
    let liste = liste_de_la_route(req.uri().path());
    let contenus = route_a_contenus(req.uri().path());
    let fiche = fiche_de_la_route(req.uri().path());
    let favoris = favoris_de_la_route(req.uri().path());
    if liste.is_none() && !contenus && fiche.is_none() && !favoris {
        return next.run(req).await;
    }
    let lang = crate::i18n::lang_from_header(req.headers());
    let (mut parts, body) = req.into_parts();
    let ActiveProfile(profil) = match ActiveProfile::from_request_parts(&mut parts, &state).await {
        Ok(p) => p,
        Err(never) => match never {},
    };
    let req = Request::from_parts(parts, body);
    let (politique, revision) = match pp::lire(&state.backend, profil) {
        Ok(Some(p)) => (p.politique, p.revision),
        Ok(None) => return next.run(req).await,
        // Sans table, aucune politique n'a pu être posée.
        Err(e) if pp::table_absente(&e) => {
            tracing::warn!(error = %e, "politique_parentale_table_absente");
            return next.run(req).await;
        }
        // Une politique illisible ne doit pas laisser passer la liste entière.
        Err(e) => return interne(e),
    };
    let filtre = Filtre::depuis(&politique);
    // #7913 — toutes les routes de la bibliothèque passent désormais par le
    // filtre des artistes : sans artiste bloqué, elles n'ont rien à relire.
    let liste = liste.filter(|l| {
        !matches!(l, Liste::Bibliotheque | Liste::Artistes | Liste::Services)
            || filtre.bloque_des_artistes()
    });
    let favoris = favoris && filtre.bloque_des_artistes();
    if liste.is_none() && fiche.is_none() && !favoris && !politique.masquer_explicite {
        return next.run(req).await;
    }
    // #8088 — les marques explicites de la bibliothèque locale, lues une
    // fois par requête. Une erreur refuse la liste plutôt que de la laisser
    // passer entière.
    let marques = if politique.masquer_explicite {
        match pp::marques_explicites_locales(&state.backend) {
            Ok(m) => Some(m),
            Err(e) => return interne(e),
        }
    } else {
        None
    };
    // #7913 — de quoi relire la liste pour son total exact.
    let gabarit = (
        req.uri().clone(),
        req.headers().clone(),
        req.extensions().clone(),
    );
    let suite = next.clone();
    let reponse = next.run(req).await;
    let est_json = reponse
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("application/json"));
    if !reponse.status().is_success() || !est_json {
        return reponse;
    }
    let (mut parts, body) = reponse.into_parts();
    let octets = match axum::body::to_bytes(body, CORPS_MAX).await {
        Ok(o) => o,
        Err(e) => return interne(e.to_string()),
    };
    let Ok(mut valeur) = serde_json::from_slice::<Value>(&octets) else {
        return Response::from_parts(parts, Body::from(octets));
    };
    // #7913 — la fiche d'un élément bloqué : refusée, pas vidée.
    if let Some(f) = fiche
        && filtre.fiche_bloquee(&valeur, f)
    {
        tracing::debug!(profil, fiche = ?f, "politique_parentale_fiche_bloquee_refusee");
        return refus_contenu_bloque(&lang);
    }
    // #8611 — les fiches des services (album, artiste, piste) : la racine,
    // reconnue par le nom de son artiste.
    if liste == Some(Liste::Services)
        && let Some(f) = fiche_de_service(&gabarit.0)
        && filtre.fiche_bloquee_de_service(&valeur, f)
    {
        tracing::debug!(profil, "politique_parentale_fiche_de_service_refusee");
        return refus_contenu_bloque(&lang);
    }
    // #8611 — les favoris de la bibliothèque : leur artiste se lit en base,
    // par lectures ciblées.
    let mut retires_favoris = 0usize;
    if favoris && let Value::Array(elements) = &mut valeur {
        let couples: Vec<(String, i64)> = elements
            .iter()
            .filter_map(|e| {
                Some((
                    e.get("item_type")?.as_str()?.to_string(),
                    e.get("item_id")?.as_i64()?,
                ))
            })
            .collect();
        let bloques = match pp::favoris_bloques(&state.backend, &filtre, &couples) {
            Ok(b) => b,
            Err(e) => return interne(e),
        };
        if !bloques.is_empty() {
            let avant = elements.len();
            elements.retain(|e| {
                let cle = e
                    .get("item_type")
                    .and_then(Value::as_str)
                    .zip(e.get("item_id").and_then(Value::as_i64));
                !cle.is_some_and(|(t, i)| bloques.contains(&(t.to_string(), i)))
            });
            retires_favoris = avant - elements.len();
        }
    }
    let total_servi = valeur.get("total").and_then(Value::as_u64);
    let mut retires = liste.map_or(0, |l| filtre.filtrer(&mut valeur, l)) + retires_favoris;
    // #7913 — le total d'une liste paginée compte seulement ce que le profil
    // peut voir, sur TOUTES les pages.
    if let (Some(l @ (Liste::Bibliotheque | Liste::Artistes)), Some(total)) = (liste, total_servi)
        && filtre.bloque_des_artistes()
        && valeur.get("items").is_some_and(Value::is_array)
        && let Some(bloques) = bloques_avec_cache(
            &state,
            (profil, chemin_sans_fenetre(&gabarit.0), revision),
            &suite,
            &gabarit,
            valeur.get("seed"),
            &filtre,
            l,
            total,
        )
        .await
    {
        valeur["total"] = Value::from(total.saturating_sub(bloques));
        retires = retires.max(1);
    }
    if let Some(m) = &marques {
        // La fiche d'un album ou d'une piste explicite : refusée, pas vidée.
        if m.racine_explicite(&valeur) {
            tracing::debug!(profil, "politique_parentale_fiche_explicite_refusee");
            return refus_explicite(&lang);
        }
        retires += m.filtrer(&mut valeur);
    }
    if retires == 0 {
        return Response::from_parts(parts, Body::from(octets));
    }
    tracing::debug!(profil, retires, "politique_parentale_liste_filtree");
    parts.headers.remove(header::CONTENT_LENGTH);
    Response::from_parts(parts, Body::from(valeur.to_string()))
}

#[cfg(test)]
mod tests_8088 {
    use super::route_a_contenus;

    #[test]
    fn les_routes_de_contenus_8088() {
        for c in [
            "/api/v1/library/albums",
            "/api/v1/library/albums/12",
            "/api/v1/search",
            "/api/v1/streaming/qobuz/search",
            "/api/v1/streaming/tidal/albums/9/tracks",
            "/api/v1/playlists/3/tracks",
            "/api/v1/playlists",
        ] {
            assert!(route_a_contenus(c), "{c}");
        }
        for c in [
            "/api/v1/radios",
            "/api/v1/podcasts/search",
            "/api/v1/zones",
            "/api/v1/libraryx",
            "/api/v1/streamingfoo/x",
            "/api/v1/profiles/2/politique-parentale",
        ] {
            assert!(!route_a_contenus(c), "{c}");
        }
    }
}

#[cfg(test)]
mod tests_8611 {
    use super::{
        Liste, favoris_de_la_route, fiche_de_service, flux_de_la_route, liste_de_la_route,
    };

    #[test]
    fn les_listes_restantes_sont_filtrees_8611() {
        for c in [
            "/api/v1/playlists/3/tracks",
            "/api/v1/playlists/3",
            "/api/v1/playlists/collaborative/4/tracks",
            "/api/v1/history",
            "/api/v1/history/top-tracks",
            "/api/v1/profiles/2/history",
        ] {
            assert_eq!(liste_de_la_route(c), Some(Liste::Bibliotheque), "{c}");
        }
        for c in [
            "/api/v1/streaming/qobuz/search",
            "/api/v1/streaming/tidal/albums/9",
            "/api/v1/profiles/2/favorites/streaming",
        ] {
            assert_eq!(liste_de_la_route(c), Some(Liste::Services), "{c}");
        }
        assert!(favoris_de_la_route("/api/v1/profiles/2/favorites"));
        assert!(!favoris_de_la_route(
            "/api/v1/profiles/2/favorites/streaming"
        ));
        assert!(!favoris_de_la_route("/api/v1/profiles/2/favorites/facets"));
        assert_eq!(liste_de_la_route("/api/v1/historyx"), None);
        assert_eq!(
            liste_de_la_route("/api/v1/profiles/2/favorites/facets"),
            None
        );
    }

    #[test]
    fn le_flux_d_une_piste_par_son_id_8611() {
        let d = flux_de_la_route("/api/v1/library/tracks/700/audio").unwrap();
        assert_eq!(d.track_id, Some(700));
        let d = flux_de_la_route("/api/v1/streaming/qobuz/tracks/123/url").unwrap();
        assert_eq!(d.source.as_deref(), Some("qobuz"));
        assert_eq!(d.source_id.as_deref(), Some("123"));
        assert!(flux_de_la_route("/api/v1/library/tracks/700").is_none());
        assert!(flux_de_la_route("/api/v1/library/tracks/abc/audio").is_none());
        let fiche = |c: &str| fiche_de_service(&c.parse().unwrap());
        assert_eq!(fiche("/api/v1/streaming/qobuz/artists/5"), Some(true));
        assert_eq!(fiche("/api/v1/streaming/qobuz/albums/5"), Some(false));
        assert_eq!(fiche("/api/v1/streaming/qobuz/albums/5/tracks"), None);
    }
}

#[cfg(test)]
mod tests_7913 {
    use super::{Fiche, Liste, fiche_de_la_route, liste_de_la_route};

    #[test]
    fn les_rangees_et_sous_listes_sont_filtrees_7913() {
        for c in [
            "/api/v1/home",
            "/api/v1/home/recently-added",
            "/api/v1/library/genres/Rock/albums",
            "/api/v1/library/collections/3/albums",
            "/api/v1/library/smart-playlists/2/tracks",
            "/api/v1/library/smart-collections",
            "/api/v1/library/artists/4/albums",
        ] {
            assert_eq!(liste_de_la_route(c), Some(Liste::Bibliotheque), "{c}");
        }
        assert_eq!(
            liste_de_la_route("/api/v1/library/artists"),
            Some(Liste::Artistes)
        );
        assert_eq!(liste_de_la_route("/api/v1/homeassistant/x"), None);
        assert_eq!(liste_de_la_route("/api/v1/zones"), None);
    }

    #[test]
    fn les_fiches_ouvertes_directement_7913() {
        assert_eq!(
            fiche_de_la_route("/api/v1/library/albums/12"),
            Some(Fiche::Album)
        );
        assert_eq!(
            fiche_de_la_route("/api/v1/library/artists/7"),
            Some(Fiche::Artiste)
        );
        assert_eq!(
            fiche_de_la_route("/api/v1/library/tracks/9"),
            Some(Fiche::Piste)
        );
        assert_eq!(fiche_de_la_route("/api/v1/radios/5"), Some(Fiche::Radio));
        assert_eq!(
            fiche_de_la_route("/api/v1/podcasts/episodes/8"),
            Some(Fiche::Podcast)
        );
        assert_eq!(
            fiche_de_la_route("/api/v1/podcasts/episodes"),
            Some(Fiche::Podcast)
        );
        for c in [
            "/api/v1/library/albums",
            "/api/v1/library/albums/12/tracks",
            "/api/v1/radios/search",
            "/api/v1/radios/favorites",
        ] {
            assert_eq!(fiche_de_la_route(c), None, "{c}");
        }
    }
}
