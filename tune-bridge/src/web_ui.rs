//! Service du client web depuis le relais.
//!
//! Le relais transportait deja l'API, le WebSocket et les flux audio, mais ne
//! servait AUCUNE interface : sa table de routes se limitait a `/health`, aux
//! tunnels et aux proxys. Ouvrir `https://bridge.mozaiklabs.fr/` dans un
//! navigateur donnait `404`. Il etait une tuyauterie sans destination.
//!
//! Les fichiers sont servis sous `/{server_id}/`, la meme adresse que le
//! serveur annonce deja lui-meme dans `access_url`
//! (`tune-server/src/routes/cloud.rs`). Le contrat existait ; il n'etait pas
//! honore.
//!
//! Le repertoire vient de `TUNE_BRIDGE_WEB_DIR`. **Absent ou introuvable, ces
//! routes ne sont pas montees du tout** : le relais se comporte exactement
//! comme avant, plutot que de servir des 404 depuis un chemin vide. Un
//! deploiement qui oublie le volume ne change donc rien au comportement
//! existant.

use std::path::{Path, PathBuf};

use axum::Router;
use axum::extract::Path as AxumPath;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::get;
use tracing::{info, warn};

/// Variable d'environnement portant le repertoire du client web construit.
pub const WEB_DIR_ENV: &str = "TUNE_BRIDGE_WEB_DIR";

/// Repertoire du client web, s'il est utilisable.
///
/// `None` couvre les trois cas ou il vaut mieux ne rien monter : variable
/// absente, repertoire inexistant, ou `index.html` manquant. Ce dernier merite
/// d'etre verifie : un volume monte mais vide donnerait une application qui se
/// charge a moitie, ce qui est plus difficile a diagnostiquer qu'une absence
/// franche.
pub fn repertoire_web() -> Option<PathBuf> {
    let brut = std::env::var(WEB_DIR_ENV).ok()?;
    let brut = brut.trim();
    if brut.is_empty() {
        return None;
    }
    let dir = PathBuf::from(brut);
    if !dir.is_dir() {
        warn!(dir = %dir.display(), "bridge_web_dir_introuvable — client web non servi");
        return None;
    }
    if !dir.join("index.html").is_file() {
        warn!(
            dir = %dir.display(),
            "bridge_web_dir_sans_index — client web non servi, le volume est vide ou incomplet"
        );
        return None;
    }
    Some(dir)
}

/// Un segment ne peut etre un identifiant de serveur que s'il en a la forme.
///
/// Sans ce controle, la route attrape-tout avalerait n'importe quel chemin de
/// premier niveau et repondrait l'application a la place d'un 404 honnete —
/// y compris pour des chemins que le relais pourrait exposer plus tard.
///
/// Les identifiants sont des UUID v4 (`TelemetryReporter::get_or_create_server_id`).
/// On verifie la forme, pas l'existence : un serveur endormi doit pouvoir
/// afficher l'interface, qui dira elle-meme qu'elle ne le joint pas. Refuser
/// ici donnerait une page blanche sans explication.
pub fn ressemble_a_un_server_id(segment: &str) -> bool {
    segment.len() == 36
        && segment.as_bytes().iter().enumerate().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => *c == b'-',
            _ => c.is_ascii_hexdigit(),
        })
}

/// Monte le service du client web, ou renvoie le routeur inchange.
///
/// Generique sur l'etat : ces routes n'en ont pas besoin, et l'imposer
/// obligerait a le nommer ici pour rien.
pub fn monter<S>(routeur: Router<S>) -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    let Some(dir) = repertoire_web() else {
        info!("bridge_web_ui_non_montee — TUNE_BRIDGE_WEB_DIR absent ou inutilisable");
        return routeur;
    };
    info!(dir = %dir.display(), "bridge_web_ui_montee");

    routeur
        // `/{id}` sans barre finale : sans cette redirection, les URL
        // relatives de la page se resoudraient a la racine du relais et
        // toutes les ressources tomberaient en 404.
        .route("/{server_id}", get(rediriger_vers_racine))
        .route("/{server_id}/", get(servir_index))
        .route("/{server_id}/{*chemin}", get(servir_fichier))
}

async fn rediriger_vers_racine(AxumPath(server_id): AxumPath<String>) -> Response {
    if !ressemble_a_un_server_id(&server_id) {
        return StatusCode::NOT_FOUND.into_response();
    }
    Redirect::permanent(&format!("/{server_id}/")).into_response()
}

async fn servir_index(AxumPath(server_id): AxumPath<String>) -> Response {
    if !ressemble_a_un_server_id(&server_id) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let Some(dir) = repertoire_web() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    servir(&dir.join("index.html")).await
}

async fn servir_fichier(AxumPath((server_id, chemin)): AxumPath<(String, String)>) -> Response {
    if !ressemble_a_un_server_id(&server_id) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let Some(dir) = repertoire_web() else {
        return StatusCode::NOT_FOUND.into_response();
    };

    // Repli SPA : une route interne de l'application (`/{id}/library`) n'a
    // aucun fichier derriere elle. On rend `index.html`, et le routeur du
    // client fait le reste — sinon un rechargement de page en pleine
    // navigation tomberait en 404.
    let cible = dir.join(&chemin);
    if chemin_est_sur(&dir, &cible) && cible.is_file() {
        servir(&cible).await
    } else {
        servir(&dir.join("index.html")).await
    }
}

/// Le chemin demande reste-t-il DANS le repertoire servi ?
///
/// `ServeDir` fait ce controle lui-meme, mais ici les fichiers sont ouverts un
/// par un : sans cette verification, un `../` remonterait l'arborescence du
/// conteneur. Le repli SPA masquerait la tentative en rendant `index.html`,
/// donc l'attaque serait silencieuse — raison de plus pour la barrer.
fn chemin_est_sur(racine: &Path, cible: &Path) -> bool {
    match (racine.canonicalize(), cible.canonicalize()) {
        (Ok(r), Ok(c)) => c.starts_with(r),
        // Cible inexistante : le repli SPA s'en charge, rien a ouvrir.
        _ => false,
    }
}

/// Type MIME d'apres l'extension.
///
/// Liste courte et explicite plutot qu'une dependance : un client web construit
/// ne contient qu'une poignee de types, et un `application/octet-stream` sur un
/// module JavaScript empeche le navigateur de l'executer — panne muette qu'il
/// vaut mieux ne pas risquer pour economiser huit lignes.
fn type_mime(chemin: &Path) -> &'static str {
    match chemin
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "woff2" => "font/woff2",
        "woff" => "font/woff",
        "map" => "application/json",
        _ => "application/octet-stream",
    }
}

async fn servir(fichier: &Path) -> Response {
    match tokio::fs::read(fichier).await {
        Ok(octets) => (
            [(axum::http::header::CONTENT_TYPE, type_mime(fichier))],
            octets,
        )
            .into_response(),
        Err(e) => {
            warn!(fichier = %fichier.display(), error = %e, "bridge_web_fichier_illisible");
            StatusCode::NOT_FOUND.into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn un_uuid_est_reconnu() {
        assert!(ressemble_a_un_server_id(
            "75f24b9e-fb8a-4de2-8007-99edd3454263"
        ));
        assert!(ressemble_a_un_server_id(
            "C7744444-98C9-4390-B81E-B20794A95046"
        ));
    }

    /// Le point de ce controle : la route attrape-tout ne doit pas avaler les
    /// chemins que le relais expose deja, ni ceux qu'il exposera.
    #[test]
    fn les_chemins_du_relais_ne_sont_pas_pris_pour_des_identifiants() {
        for segment in ["api", "ws", "stream", "health", "", "favicon.ico"] {
            assert!(
                !ressemble_a_un_server_id(segment),
                "{segment:?} ne doit pas passer pour un identifiant de serveur"
            );
        }
    }

    #[test]
    fn une_forme_approchante_est_refusee() {
        // bonne longueur, tirets mal places
        assert!(!ressemble_a_un_server_id(
            "75f24b9efb8a-4de2-8007-99edd34542-63"
        ));
        // bonne forme, caractere non hexadecimal
        assert!(!ressemble_a_un_server_id(
            "75f24b9e-fb8a-4de2-8007-99edd345426z"
        ));
        // trop court
        assert!(!ressemble_a_un_server_id("75f24b9e-fb8a-4de2-8007"));
    }

    #[test]
    fn sans_variable_denvironnement_rien_nest_monte() {
        // SAFETY: test mono-thread sur cette variable.
        unsafe { std::env::remove_var(WEB_DIR_ENV) };
        assert!(repertoire_web().is_none());
    }

    #[test]
    fn un_repertoire_inexistant_ne_monte_rien() {
        unsafe { std::env::set_var(WEB_DIR_ENV, "/n/existe/pas/du/tout") };
        assert!(repertoire_web().is_none());
        unsafe { std::env::remove_var(WEB_DIR_ENV) };
    }

    /// Un volume monte mais vide donnerait une application a moitie chargee,
    /// plus difficile a diagnostiquer qu'une absence franche.
    #[test]
    fn un_repertoire_sans_index_ne_monte_rien() {
        let tmp = tempfile::TempDir::new().unwrap();
        unsafe { std::env::set_var(WEB_DIR_ENV, tmp.path().to_str().unwrap()) };
        assert!(repertoire_web().is_none());
        unsafe { std::env::remove_var(WEB_DIR_ENV) };
    }
}
