//! `/api/v1/sources-reseau` — les sources lues par le moteur sans montage
//! (#8189, étape 2 : le socle ; WebDAV, SMB et NFS arrivent aux étapes 3, 4
//! et 6).
//!
//! - `GET    /sources-reseau` : les sources configurées et les types pris en
//!   charge ;
//! - `POST   /sources-reseau` : en ajouter une (identifiants scellés aussitôt) ;
//! - `GET    /sources-reseau/{id}` / `DELETE` ;
//! - `POST   /sources-reseau/{id}/tester` : joindre la racine et la lister ;
//! - `POST   /sources-reseau/{id}/rescanner` : scan ciblé de sa racine ;
//! - `GET    /sources-reseau/{id}/parcourir?chemin=` : un dossier distant ;
//! - `POST   /sources-reseau/{id}/lien?chemin=` : l'URL signée d'un fichier ;
//! - `GET    /sources-reseau/{id}/fichier?chemin=&jeton=` : la PASSERELLE,
//!   avec `Range`, pour un renderer qui lit une URL et ne sait présenter
//!   aucun identifiant.
//!
//! Les erreurs se lisent `{code, message}` — `message` en anglais, pour le
//! journal ; l'écran traduit le CODE (règle `garde_francais_api`) :
//! `type_non_pris_en_charge`,
//! `cle_de_l_hote_absente`, `identifiants_refuses`, `injoignable`,
//! `introuvable`, `source_inconnue`, `jeton_refuse`…
//!
//! Une piste d'une source distante se joue, elle, par la route ordinaire de la
//! piste (`/library/tracks/{id}/stream`) : son chemin `tune-src://…` passe par
//! `iso9660::ouvrir_fichier`, comme celui d'une image ISO.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde::Deserialize;
use serde_json::{Value, json};

use tune_core::db::sources_reseau::{NouvelleSource, SourceReseau, SourcesReseauRepo};
use tune_core::sources_reseau::{self as sr, identifiants};

use crate::state::AppState;

pub fn router() -> axum::Router<AppState> {
    axum::Router::new()
        .route("/", get(lister).post(creer))
        .route("/{id}", get(obtenir).delete(supprimer))
        .route("/{id}/tester", post(tester))
        .route("/{id}/rescanner", post(rescanner))
        .route("/{id}/parcourir", get(parcourir))
        .route("/{id}/lien", post(lien))
        .route("/{id}/fichier", get(passerelle).head(passerelle))
}

/// Le chemin de la passerelle, seul exempté du jeton de session par
/// `auth_middleware` : le gestionnaire exige le jeton signé du fichier.
pub(crate) fn est_route_passerelle(chemin: &str) -> bool {
    let reste = chemin
        .strip_prefix("/api/v1/sources-reseau/")
        .or_else(|| chemin.strip_prefix("/sources-reseau/"));
    match reste.and_then(|r| r.split_once('/')) {
        Some((id, "fichier")) => sr::identifiant_valide(id),
        _ => false,
    }
}

fn erreur(statut: StatusCode, code: &str, message: &str) -> Response {
    (statut, Json(json!({ "code": code, "message": message }))).into_response()
}

fn erreur_io(e: &std::io::Error) -> Response {
    let code = sr::systeme::code_d_erreur(e);
    let (statut, message) = match code {
        "identifiants_refuses" => (
            StatusCode::FORBIDDEN,
            "The source rejected these credentials.",
        ),
        "introuvable" => (
            StatusCode::NOT_FOUND,
            "This file or folder does not exist on the source.",
        ),
        "injoignable" => (
            StatusCode::SERVICE_UNAVAILABLE,
            "The source is not responding.",
        ),
        _ => (StatusCode::BAD_GATEWAY, "The source returned an error."),
    };
    erreur(statut, code, message)
}

fn vue(s: &SourceReseau) -> Value {
    let mut v = serde_json::to_value(s).unwrap_or_default();
    if let Some(o) = v.as_object_mut() {
        o.insert("racine_virtuelle".into(), json!(s.racine_virtuelle()));
        o.insert("identifiants".into(), json!(s.a_des_identifiants()));
        o.insert("disponible".into(), json!(sr::obtenir(&s.id).is_some()));
    }
    v
}

fn repo(state: &AppState) -> SourcesReseauRepo {
    SourcesReseauRepo::with_backend(state.backend.clone())
}

// ─── Ouverture des sources ─────────────────────────────────────────────

/// Construit et enregistre le système de la source `s`. Rend l'état à
/// retenir en base.
pub(crate) fn ouvrir_la_source(
    state: &AppState,
    s: &SourceReseau,
) -> Result<(), (&'static str, String)> {
    let Some(fabrique) = sr::fabrique(&s.type_source) else {
        sr::retirer(&s.id);
        return Err(("non_prise_en_charge", "type_non_pris_en_charge".into()));
    };
    let ids = match &s.identifiants_scelles {
        Some(scelle) => match identifiants::ouvrir(scelle, state.config.mobile) {
            Ok(i) => Some(i),
            Err(e) => {
                sr::retirer(&s.id);
                return Err(("identifiants_illisibles", e.code().into()));
            }
        },
        None => None,
    };
    match fabrique(s, ids.as_ref()) {
        Ok(systeme) => {
            sr::enregistrer(&s.id, systeme);
            brancher_la_persistance(state, &s.id);
            if let Some(point) = s
                .reprise_scan
                .as_deref()
                .and_then(|j| serde_json::from_str::<sr::PointDeReprise>(j).ok())
            {
                sr::restaurer_la_reprise(&s.id, point);
            }
            Ok(())
        }
        Err(e) => {
            sr::retirer(&s.id);
            Err(("injoignable", sr::systeme::code_d_erreur(&e).into()))
        }
    }
}

/// Garde les points de reprise de la source `id` dans la table, et en tire
/// son état : parcours réussi, `a_jour` ; interrompu, `injoignable` (il
/// reprendra où il s'est arrêté).
fn brancher_la_persistance(state: &AppState, id: &str) {
    let backend = state.backend.clone();
    sr::installer_la_persistance(
        id,
        std::sync::Arc::new(move |id: &str, point: Option<&sr::PointDeReprise>| {
            let repo = SourcesReseauRepo::with_backend(backend.clone());
            let json = point.and_then(|p| serde_json::to_string(p).ok());
            let _ = repo.poser_reprise(id, json.as_deref());
            match point {
                None => {
                    let _ = repo.poser_etat(id, "a_jour", None);
                    let _ = repo.noter_le_scan(id);
                }
                Some(_) => {
                    let _ = repo.poser_etat(id, "injoignable", Some("injoignable"));
                }
            }
        }),
    );
}

/// Au démarrage : enregistre chaque source configurée.
pub fn demarrer(state: &AppState) {
    for s in repo(state).lister().unwrap_or_default() {
        if let Err((etat, code)) = ouvrir_la_source(state, &s) {
            tracing::warn!(source = %s.id, type_source = %s.type_source, etat, code = %code, "source_reseau_non_ouverte");
            let _ = repo(state).poser_etat(&s.id, etat, Some(&code));
        }
    }
}

// ─── Routes ─────────────────────────────────────────────────────────────

async fn lister(State(state): State<AppState>) -> Response {
    match repo(&state).lister() {
        Ok(l) => Json(json!({
            "sources": l.iter().map(vue).collect::<Vec<_>>(),
            "types": sr::types_pris_en_charge(),
        }))
        .into_response(),
        Err(e) => erreur(StatusCode::INTERNAL_SERVER_ERROR, "base", &e),
    }
}

#[derive(Deserialize)]
struct CorpsIdentifiants {
    #[serde(default)]
    utilisateur: String,
    #[serde(default)]
    mot_de_passe: String,
    #[serde(default)]
    domaine: Option<String>,
}

#[derive(Deserialize)]
struct CorpsCreation {
    #[serde(rename = "type")]
    type_source: String,
    #[serde(default)]
    nom: String,
    #[serde(default)]
    hote: String,
    #[serde(default)]
    port: Option<i64>,
    #[serde(default)]
    partage: String,
    #[serde(default)]
    racine: String,
    #[serde(default)]
    identifiants: Option<CorpsIdentifiants>,
}

async fn creer(State(state): State<AppState>, Json(c): Json<CorpsCreation>) -> Response {
    if sr::fabrique(&c.type_source).is_none() {
        return erreur(
            StatusCode::BAD_REQUEST,
            "type_non_pris_en_charge",
            "This source type is not supported by this server yet.",
        );
    }
    if c.racine.split('/').any(|p| p == "..") {
        return erreur(StatusCode::BAD_REQUEST, "racine_invalide", "Invalid root.");
    }
    let scelle = match c.identifiants {
        Some(i) if !i.utilisateur.is_empty() || !i.mot_de_passe.is_empty() => {
            let ids = identifiants::Identifiants {
                utilisateur: i.utilisateur,
                mot_de_passe: i.mot_de_passe,
                domaine: i.domaine.filter(|d| !d.trim().is_empty()),
            };
            match identifiants::sceller(&ids, state.config.mobile) {
                Ok(s) => Some(s),
                Err(e) => {
                    return erreur(
                        StatusCode::SERVICE_UNAVAILABLE,
                        e.code(),
                        "Credentials cannot be protected on this device.",
                    );
                }
            }
        }
        _ => None,
    };
    let nom = if c.nom.trim().is_empty() {
        c.hote.clone()
    } else {
        c.nom.trim().to_string()
    };
    let cree = match repo(&state).creer(&NouvelleSource {
        type_source: c.type_source,
        nom,
        hote: c.hote,
        port: c.port,
        partage: c.partage,
        racine: c.racine,
        identifiants_scelles: scelle,
    }) {
        Ok(s) => s,
        Err(e) => return erreur(StatusCode::INTERNAL_SERVER_ERROR, "base", &e),
    };
    if let Err((etat, code)) = ouvrir_la_source(&state, &cree) {
        let _ = repo(&state).poser_etat(&cree.id, etat, Some(&code));
    }
    let relue = repo(&state)
        .obtenir(&cree.id)
        .ok()
        .flatten()
        .unwrap_or(cree);
    (StatusCode::CREATED, Json(vue(&relue))).into_response()
}

fn source_ou_404(state: &AppState, id: &str) -> Result<SourceReseau, Response> {
    match repo(state).obtenir(id) {
        Ok(Some(s)) => Ok(s),
        Ok(None) => Err(erreur(
            StatusCode::NOT_FOUND,
            "source_inconnue",
            "Unknown source.",
        )),
        Err(e) => Err(erreur(StatusCode::INTERNAL_SERVER_ERROR, "base", &e)),
    }
}

async fn obtenir(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    match source_ou_404(&state, &id) {
        Ok(s) => Json(vue(&s)).into_response(),
        Err(r) => r,
    }
}

async fn supprimer(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    sr::retirer(&id);
    match repo(&state).supprimer(&id) {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => erreur(StatusCode::NOT_FOUND, "source_inconnue", "Unknown source."),
        Err(e) => erreur(StatusCode::INTERNAL_SERVER_ERROR, "base", &e),
    }
}

/// Le système enregistré de la source, ou la réponse qui dit pourquoi non.
fn systeme_ou_erreur(
    state: &AppState,
    id: &str,
) -> Result<(SourceReseau, std::sync::Arc<dyn sr::SystemeDistant>), Response> {
    let s = source_ou_404(state, id)?;
    match sr::obtenir(id) {
        Some(sys) => Ok((s, sys)),
        None => Err(erreur(
            StatusCode::SERVICE_UNAVAILABLE,
            if s.etat == "non_prise_en_charge" {
                "type_non_pris_en_charge"
            } else {
                "source_non_ouverte"
            },
            "This source is not open on this server.",
        )),
    }
}

async fn bloquant<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Option<T> {
    tokio::task::spawn_blocking(f).await.ok()
}

async fn tester(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let (s, sys) = match systeme_ou_erreur(&state, &id) {
        Ok(x) => x,
        Err(r) => return r,
    };
    let racine = s.racine.clone();
    match bloquant(move || sys.lister(&racine)).await {
        Some(Ok(entrees)) => {
            let _ = repo(&state).poser_etat(
                &id,
                if s.dernier_scan.is_some() {
                    "a_jour"
                } else {
                    "nouvelle"
                },
                None,
            );
            Json(json!({ "ok": true, "entrees": entrees.len() })).into_response()
        }
        Some(Err(e)) => {
            let code = sr::systeme::code_d_erreur(&e);
            let _ = repo(&state).poser_etat(&id, code, Some(code));
            erreur_io(&e)
        }
        None => erreur(
            StatusCode::INTERNAL_SERVER_ERROR,
            "erreur",
            "Test interrupted.",
        ),
    }
}

async fn rescanner(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let (s, _) = match systeme_ou_erreur(&state, &id) {
        Ok(x) => x,
        Err(r) => return r,
    };
    let _ = repo(&state).poser_etat(&id, "indexation", None);
    let lance = crate::routes::system::scan::spawn_library_scan(
        state.clone(),
        false,
        Some(s.racine_virtuelle()),
    )
    .await;
    (
        StatusCode::ACCEPTED,
        Json(json!({ "lance": lance, "racine": s.racine_virtuelle() })),
    )
        .into_response()
}

#[derive(Deserialize)]
struct QueryChemin {
    #[serde(default)]
    chemin: String,
    #[serde(default)]
    jeton: String,
}

/// Le chemin interne demandé, joint à la racine de la source, sans `..`.
fn chemin_sous_la_racine(s: &SourceReseau, demande: &str) -> Option<String> {
    let demande = demande.trim_matches('/');
    if demande.split('/').any(|p| p == "..") {
        return None;
    }
    Some(match (s.racine.is_empty(), demande.is_empty()) {
        (true, _) => demande.to_string(),
        (false, true) => s.racine.clone(),
        (false, false) => format!("{}/{demande}", s.racine),
    })
}

async fn parcourir(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<QueryChemin>,
) -> Response {
    let (s, sys) = match systeme_ou_erreur(&state, &id) {
        Ok(x) => x,
        Err(r) => return r,
    };
    let Some(dossier) = chemin_sous_la_racine(&s, &q.chemin) else {
        return erreur(StatusCode::BAD_REQUEST, "chemin_invalide", "Invalid path.");
    };
    match bloquant(move || sys.lister(&dossier)).await {
        Some(Ok(mut e)) => {
            e.sort_by(|a, b| (!a.dossier, &a.nom).cmp(&(!b.dossier, &b.nom)));
            Json(json!({ "chemin": q.chemin.trim_matches('/'), "entrees": e })).into_response()
        }
        Some(Err(e)) => erreur_io(&e),
        None => erreur(
            StatusCode::INTERNAL_SERVER_ERROR,
            "erreur",
            "Listing interrupted.",
        ),
    }
}

async fn lien(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<QueryChemin>,
) -> Response {
    let s = match source_ou_404(&state, &id) {
        Ok(s) => s,
        Err(r) => return r,
    };
    let Some(interne) = chemin_sous_la_racine(&s, &q.chemin).filter(|c| !c.is_empty()) else {
        return erreur(StatusCode::BAD_REQUEST, "chemin_invalide", "Invalid path.");
    };
    let jeton = sr::jeton::signer(&id, &interne, sr::jeton::DUREE_PAR_DEFAUT);
    let chemin_url = format!(
        "/api/v1/sources-reseau/{id}/fichier?chemin={}&jeton={}",
        encoder(&interne),
        encoder(&jeton)
    );
    let base = format!("http://{}:{}", state.config.server_ip(), state.config.port);
    Json(json!({
        "chemin": chemin_url,
        "url": format!("{base}{chemin_url}"),
        "expire_dans_s": sr::jeton::DUREE_PAR_DEFAUT.as_secs(),
    }))
    .into_response()
}

fn encoder(s: &str) -> String {
    let mut sortie = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~/".contains(&b) {
            sortie.push(b as char);
        } else {
            sortie.push_str(&format!("%{b:02X}"));
        }
    }
    sortie
}

/// `Range: bytes=a-b` (une seule plage). `None` : en-tête absent ou
/// illisible, tout le fichier. `Some(Err)` : plage hors du fichier.
fn plage(entetes: &HeaderMap, taille: u64) -> Option<Result<(u64, u64), ()>> {
    let v = entetes.get(header::RANGE)?.to_str().ok()?.trim();
    let spec = v.strip_prefix("bytes=")?;
    if spec.contains(',') {
        return None;
    }
    let (a, b) = spec.split_once('-')?;
    let r = match (a.trim(), b.trim()) {
        ("", n) => {
            let n: u64 = n.parse().ok()?;
            if n == 0 || taille == 0 {
                return Some(Err(()));
            }
            (taille.saturating_sub(n), taille - 1)
        }
        (a, "") => (a.parse().ok()?, taille.saturating_sub(1)),
        (a, b) => (
            a.parse().ok()?,
            b.parse::<u64>().ok()?.min(taille.saturating_sub(1)),
        ),
    };
    if r.0 >= taille || r.0 > r.1 {
        return Some(Err(()));
    }
    Some(Ok(r))
}

fn type_mime(chemin: &str) -> &'static str {
    let ext = chemin.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "flac" => "audio/flac",
        "mp3" => "audio/mpeg",
        "m4a" | "mp4" | "alac" | "aac" => "audio/mp4",
        "ogg" | "oga" => "audio/ogg",
        "opus" => "audio/opus",
        "wav" => "audio/wav",
        "aif" | "aiff" => "audio/aiff",
        "dsf" => "audio/x-dsf",
        "dff" => "audio/x-dff",
        "wv" => "audio/x-wavpack",
        "ape" => "audio/x-ape",
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        _ => "application/octet-stream",
    }
}

/// La passerelle : les octets d'un fichier distant, par plages, au porteur
/// du jeton de CE fichier. Ni listage, ni autre fichier.
async fn passerelle(
    State(_state): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<QueryChemin>,
    methode: axum::http::Method,
    entetes: HeaderMap,
) -> Response {
    let interne = q.chemin.trim_matches('/').to_string();
    if interne.is_empty()
        || interne.split('/').any(|p| p == "..")
        || !sr::jeton::verifier(&id, &interne, &q.jeton)
    {
        return erreur(
            StatusCode::FORBIDDEN,
            "jeton_refuse",
            "Expired or invalid link.",
        );
    }
    let virtuel = sr::chemin_distant(&id, &interne);
    let v = virtuel.clone();
    let lecteur = match bloquant(move || sr::ouvrir(&v)).await {
        Some(Ok(l)) => l,
        Some(Err(e)) => return erreur_io(&e),
        None => {
            return erreur(
                StatusCode::INTERNAL_SERVER_ERROR,
                "erreur",
                "Open interrupted.",
            );
        }
    };
    let taille = lecteur.taille();
    let (statut, debut, fin) = match plage(&entetes, taille) {
        None => (StatusCode::OK, 0, taille.saturating_sub(1)),
        Some(Ok((a, b))) => (StatusCode::PARTIAL_CONTENT, a, b),
        Some(Err(())) => {
            return (
                StatusCode::RANGE_NOT_SATISFIABLE,
                [(header::CONTENT_RANGE, format!("bytes */{taille}"))],
            )
                .into_response();
        }
    };
    let longueur = if taille == 0 { 0 } else { fin - debut + 1 };
    let mut h = HeaderMap::new();
    h.insert(header::ACCEPT_RANGES, "bytes".parse().unwrap());
    h.insert(header::CONTENT_TYPE, type_mime(&interne).parse().unwrap());
    h.insert(
        header::CONTENT_LENGTH,
        longueur.to_string().parse().unwrap(),
    );
    if statut == StatusCode::PARTIAL_CONTENT {
        h.insert(
            header::CONTENT_RANGE,
            format!("bytes {debut}-{fin}/{taille}").parse().unwrap(),
        );
    }
    if methode == axum::http::Method::HEAD || longueur == 0 {
        return (statut, h).into_response();
    }
    let (tx, mut rx) = tokio::sync::mpsc::channel::<std::io::Result<bytes::Bytes>>(4);
    tokio::task::spawn_blocking(move || {
        use std::io::{Read, Seek};
        let mut l = lecteur;
        if let Err(e) = l.seek(std::io::SeekFrom::Start(debut)) {
            let _ = tx.blocking_send(Err(e));
            return;
        }
        let mut restant = longueur;
        let mut buf = vec![0u8; 64 * 1024];
        while restant > 0 {
            let vise = buf.len().min(restant as usize);
            match l.read(&mut buf[..vise]) {
                Ok(0) => break,
                Ok(n) => {
                    restant -= n as u64;
                    if tx
                        .blocking_send(Ok(bytes::Bytes::copy_from_slice(&buf[..n])))
                        .is_err()
                    {
                        break;
                    }
                }
                Err(e) => {
                    let _ = tx.blocking_send(Err(e));
                    break;
                }
            }
        }
    });
    let flux = async_stream::stream! {
        while let Some(m) = rx.recv().await {
            yield m;
        }
    };
    (statut, h, axum::body::Body::from_stream(flux)).into_response()
}

#[cfg(test)]
#[path = "sources_reseau_tests_8189.rs"]
mod tests;
