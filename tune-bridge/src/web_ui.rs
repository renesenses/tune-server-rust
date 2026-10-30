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
//! S'y ajoutent l'adresse FIXE de retour OAuth du client, `/oauth/retour`
//! (#6742), et ses ressources `/assets/…` (et `/oauth/assets/…` pour un client
//! construit en base relative).
//!
//! Le repertoire vient de `TUNE_BRIDGE_WEB_DIR`. **Absent ou introuvable, ces
//! routes ne sont pas montees du tout** : le relais se comporte exactement
//! comme avant, plutot que de servir des 404 depuis un chemin vide. Un
//! deploiement qui oublie le volume ne change donc rien au comportement
//! existant.

use std::path::{Path, PathBuf};
use std::sync::Arc;

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
    monter_depuis(routeur, dir)
}

/// Les routes du client web, servies depuis `dir`.
///
/// Separee de [`monter`] pour que les tests montent un repertoire a eux sans
/// toucher a `TUNE_BRIDGE_WEB_DIR`, variable partagee par tous les tests du
/// processus. Le repertoire est lu une fois, au montage, puis capture par les
/// gestionnaires.
fn monter_depuis<S>(routeur: Router<S>, dir: PathBuf) -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    let dir = Arc::new(dir);
    let d_index = dir.clone();
    let d_fichier = dir.clone();
    let d_retour = dir.clone();
    let d_assets = dir.clone();
    let d_assets_oauth = dir;

    routeur
        // `/{id}` sans barre finale : sans cette redirection, les URL
        // relatives de la page se resoudraient a la racine du relais et
        // toutes les ressources tomberaient en 404.
        .route("/{server_id}", get(rediriger_vers_racine))
        .route(
            "/{server_id}/",
            get(move |AxumPath(server_id): AxumPath<String>| {
                let dir = d_index.clone();
                async move { servir_index(&dir, &server_id).await }
            }),
        )
        .route(
            "/{server_id}/{*chemin}",
            get(
                move |AxumPath((server_id, chemin)): AxumPath<(String, String)>| {
                    let dir = d_fichier.clone();
                    async move { servir_fichier(&dir, &server_id, &chemin).await }
                },
            ),
        )
        // Retour OAuth du client web (#6742, tune-web-client#2302). L'adresse
        // de retour declaree au site est FIXE (`/oauth/retour`, le
        // `server_id` voyage dans `state`) : elle ne peut pas etre sous
        // `/{server_id}/`. Sans jeton, comme `/{server_id}/` : la page ne
        // porte aucune donnee, c'est le client qui echange le code. La query
        // (`code`, `state`) reste dans l'URL du navigateur, rien a en faire
        // ici. Segment statique : axum le fait primer sur `{server_id}`.
        .route(
            "/oauth/retour",
            get(move || {
                let dir = d_retour.clone();
                async move { servir(&dir.join("index.html")).await }
            }),
        )
        // Ressources de la page. Le client publie est construit avec la base
        // par defaut de Vite (`/`) : `index.html` charge `/assets/…` a la
        // racine, quelle que soit l'adresse de la page — depuis
        // `/oauth/retour` comme depuis `/{server_id}/`. Construit en base
        // relative (`--base=./`), il chargerait `./assets/…`, soit
        // `/oauth/assets/…` depuis `/oauth/retour` : les deux sont servis.
        // PAS de repli SPA ici : une ressource absente est un 404, pas une
        // page HTML qu'un `<script type=module>` refuserait sans le dire.
        .route(
            "/assets/{*chemin}",
            get(move |AxumPath(chemin): AxumPath<String>| {
                let dir = d_assets.clone();
                async move { servir_ressource(&dir, &chemin).await }
            }),
        )
        .route(
            "/oauth/assets/{*chemin}",
            get(move |AxumPath(chemin): AxumPath<String>| {
                let dir = d_assets_oauth.clone();
                async move { servir_ressource(&dir, &chemin).await }
            }),
        )
}

async fn rediriger_vers_racine(AxumPath(server_id): AxumPath<String>) -> Response {
    if !ressemble_a_un_server_id(&server_id) {
        return StatusCode::NOT_FOUND.into_response();
    }
    Redirect::permanent(&format!("/{server_id}/")).into_response()
}

async fn servir_index(dir: &Path, server_id: &str) -> Response {
    if !ressemble_a_un_server_id(server_id) {
        return StatusCode::NOT_FOUND.into_response();
    }
    servir(&dir.join("index.html")).await
}

async fn servir_fichier(dir: &Path, server_id: &str, chemin: &str) -> Response {
    if !ressemble_a_un_server_id(server_id) {
        return StatusCode::NOT_FOUND.into_response();
    }

    // Repli SPA : une route interne de l'application (`/{id}/library`) n'a
    // aucun fichier derriere elle. On rend `index.html`, et le routeur du
    // client fait le reste — sinon un rechargement de page en pleine
    // navigation tomberait en 404.
    let cible = dir.join(chemin);
    if chemin_est_sur(dir, &cible) && cible.is_file() {
        servir(&cible).await
    } else {
        servir(&dir.join("index.html")).await
    }
}

/// Un fichier de `dir/assets/`, ou 404 — jamais de repli SPA.
async fn servir_ressource(dir: &Path, chemin: &str) -> Response {
    let assets = dir.join("assets");
    let cible = assets.join(chemin);
    if chemin_est_sur(&assets, &cible) && cible.is_file() {
        servir(&cible).await
    } else {
        StatusCode::NOT_FOUND.into_response()
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

/// Politique de contenu de la page, sans les empreintes des scripts en ligne.
///
/// - `style-src 'unsafe-inline'` : Svelte pose des attributs `style` et ses
///   transitions injectent des `<style>` ; Google Fonts sert la feuille.
/// - `img-src https:` / `media-src https:` : pochettes des services
///   (Qobuz, TIDAL, Bandcamp…), logos de radios et radios lues en direct.
///   `data:` (son de deverrouillage, icones CSS), `blob:` (avatar local).
/// - `connect-src` : le relais (`'self'`, et `wss://` ecrit pour les
///   navigateurs qui ne l'y rangent pas), le site du compte, l'API GitHub
///   des notes de version.
const CSP_PAGE: &str = "default-src 'self'; \
style-src 'self' 'unsafe-inline' https://fonts.googleapis.com; \
font-src 'self' data: https://fonts.gstatic.com; \
img-src 'self' data: blob: https:; \
media-src 'self' data: blob: https:; \
connect-src 'self' https://mozaiklabs.fr wss://bridge.mozaiklabs.fr https://api.github.com; \
worker-src 'self'; manifest-src 'self'; object-src 'none'; \
frame-ancestors 'none'; base-uri 'none'";

/// La politique d'un fichier servi : celle de la page, avec `script-src`
/// limite a `'self'` et aux empreintes des scripts en ligne du HTML (le
/// theme pose avant le rendu, l'enregistrement du service worker) et de ses
/// gestionnaires `on…=` (chargement differe des polices).
fn politique_de_contenu(html: Option<&str>) -> String {
    let mut scripts = String::from("script-src 'self'");
    if let Some(html) = html {
        let (en_ligne, gestionnaires) = scripts_en_ligne(html);
        if !gestionnaires.is_empty() {
            scripts.push_str(" 'unsafe-hashes'");
        }
        for code in en_ligne.iter().chain(gestionnaires.iter()) {
            scripts.push_str(&format!(" 'sha256-{}'", empreinte(code)));
        }
    }
    format!("{CSP_PAGE}; {scripts}")
}

fn empreinte(code: &str) -> String {
    use base64::Engine as _;
    use sha2::Digest as _;
    base64::engine::general_purpose::STANDARD.encode(sha2::Sha256::digest(code.as_bytes()))
}

/// Le texte des `<script>` sans `src`, et la valeur des attributs `on…`, tels
/// que le navigateur les hache.
fn scripts_en_ligne(html: &str) -> (Vec<String>, Vec<String>) {
    let octets = html.as_bytes();
    let minuscule = html.to_ascii_lowercase();
    let mut scripts = Vec::new();
    let mut gestionnaires = Vec::new();
    let mut i = 0;
    while let Some(p) = minuscule[i..].find('<') {
        i += p;
        if minuscule[i..].starts_with("<!--") {
            i = minuscule[i..].find("-->").map_or(html.len(), |f| i + f + 3);
            continue;
        }
        if !octets.get(i + 1).is_some_and(|c| c.is_ascii_alphabetic()) {
            i += 1;
            continue;
        }
        // Nom de la balise.
        let mut j = i + 1;
        while j < octets.len() && (octets[j].is_ascii_alphanumeric() || octets[j] == b'-') {
            j += 1;
        }
        let balise = &minuscule[i + 1..j];
        let mut avec_src = false;
        // Attributs, guillemets respectes, jusqu'au `>`.
        loop {
            while j < octets.len() && (octets[j].is_ascii_whitespace() || octets[j] == b'/') {
                j += 1;
            }
            if j >= octets.len() || octets[j] == b'>' {
                break;
            }
            let debut_nom = j;
            while j < octets.len()
                && !octets[j].is_ascii_whitespace()
                && !matches!(octets[j], b'=' | b'>' | b'/')
            {
                j += 1;
            }
            let nom = &minuscule[debut_nom..j];
            let mut valeur = None;
            if j < octets.len() && octets[j] == b'=' {
                j += 1;
                match octets.get(j) {
                    Some(&q @ (b'"' | b'\'')) => {
                        let fin = html[j + 1..]
                            .find(q as char)
                            .map_or(html.len(), |f| j + 1 + f);
                        valeur = Some(&html[j + 1..fin]);
                        j = (fin + 1).min(html.len());
                    }
                    _ => {
                        let debut = j;
                        while j < octets.len()
                            && !octets[j].is_ascii_whitespace()
                            && octets[j] != b'>'
                        {
                            j += 1;
                        }
                        valeur = Some(&html[debut..j]);
                    }
                }
            }
            if nom == "src" {
                avec_src = true;
            }
            if let (true, Some(v)) = (nom.len() > 2 && nom.starts_with("on"), valeur) {
                gestionnaires.push(decoder_entites(v));
            }
            if nom.is_empty() {
                j += 1;
            }
        }
        let fin_balise = (j + 1).min(html.len());
        if balise == "script" {
            let fin = minuscule[fin_balise..]
                .find("</script")
                .map_or(html.len(), |f| fin_balise + f);
            if !avec_src {
                scripts.push(html[fin_balise..fin].to_string());
            }
            i = fin;
        } else {
            i = fin_balise;
        }
    }
    (scripts, gestionnaires)
}

/// Les references de caracteres courantes d'une valeur d'attribut.
fn decoder_entites(v: &str) -> String {
    v.replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&#x27;", "'")
        .replace("&apos;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

async fn servir(fichier: &Path) -> Response {
    use axum::http::{HeaderValue, header};
    match tokio::fs::read(fichier).await {
        Ok(octets) => {
            let type_ = type_mime(fichier);
            let html = type_
                .starts_with("text/html")
                .then(|| String::from_utf8_lossy(&octets).into_owned());
            let politique = politique_de_contenu(html.as_deref());
            let mut r = ([(header::CONTENT_TYPE, type_)], octets).into_response();
            let entetes = r.headers_mut();
            entetes.insert(
                header::X_CONTENT_TYPE_OPTIONS,
                HeaderValue::from_static("nosniff"),
            );
            entetes.insert(
                header::REFERRER_POLICY,
                HeaderValue::from_static("no-referrer"),
            );
            if let Ok(v) = HeaderValue::from_str(&politique) {
                entetes.insert(header::CONTENT_SECURITY_POLICY, v);
            }
            r
        }
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

    // --- #6742 : retour OAuth du client web -------------------------------

    const INDEX: &str =
        "<!doctype html><title>Tune</title><script type=module src=/assets/index-abc.js></script>";
    const MODULE: &str = "console.log('tune')";
    const SERVEUR: &str = "75f24b9e-fb8a-4de2-8007-99edd3454263";

    /// Un client web construit, dans un repertoire temporaire.
    fn client_construit() -> tempfile::TempDir {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("index.html"), INDEX).unwrap();
        std::fs::create_dir(tmp.path().join("assets")).unwrap();
        std::fs::write(tmp.path().join("assets/index-abc.js"), MODULE).unwrap();
        tmp
    }

    /// Le VRAI routeur du relais (`crate::routes()`) plus le client web, sur
    /// 127.0.0.1 : la route doit tenir a cote de celles du relais, pas seule.
    async fn pont_avec_client(dir: Option<&Path>) -> String {
        let app = crate::routes();
        let app = match dir {
            Some(d) => monter_depuis(app, d.to_path_buf()),
            None => app,
        };
        let app = app.with_state(std::sync::Arc::new(crate::state::RelayState::new()));
        let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let adresse = ecoute.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(ecoute, app).await;
        });
        format!("http://{adresse}")
    }

    async fn obtenir(url: &str) -> (u16, String, String) {
        let r = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap()
            .get(url)
            .send()
            .await
            .unwrap();
        let statut = r.status().as_u16();
        let type_ = r
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .map(|v| v.to_str().unwrap().to_string())
            .unwrap_or_default();
        (statut, type_, r.text().await.unwrap())
    }

    #[tokio::test]
    async fn le_retour_oauth_sert_l_index_du_client_avec_sa_query() {
        let client = client_construit();
        let base = pont_avec_client(Some(client.path())).await;
        let (statut, type_, corps) =
            obtenir(&format!("{base}/oauth/retour?code=abc&state=xyz")).await;
        assert_eq!(
            statut, 200,
            "GET /oauth/retour?code=…&state=… doit rendre index.html du client web (#6742) : \
             sans cette route, le retour OAuth du client (tune-web-client#2302) tombe en 404"
        );
        assert_eq!(
            corps, INDEX,
            "/oauth/retour doit rendre index.html tel quel"
        );
        // Memes en-tetes que l'index servi sous `/{server_id}/`.
        let (_, type_index, _) = obtenir(&format!("{base}/{SERVEUR}/")).await;
        assert_eq!(type_, type_index);
        assert_eq!(type_, "text/html; charset=utf-8");
    }

    /// `index.html` charge ses modules a la racine (`/assets/…`, base Vite par
    /// defaut) ou, construit en base relative, sous `/oauth/assets/…` depuis
    /// `/oauth/retour`. Les deux doivent rendre le module, en JavaScript.
    #[tokio::test]
    async fn les_ressources_de_la_page_de_retour_se_resolvent() {
        let client = client_construit();
        let base = pont_avec_client(Some(client.path())).await;
        for chemin in ["/assets/index-abc.js", "/oauth/assets/index-abc.js"] {
            let (statut, type_, corps) = obtenir(&format!("{base}{chemin}")).await;
            assert_eq!(statut, 200, "{chemin} doit etre servi depuis dist/assets");
            assert_eq!(type_, "text/javascript; charset=utf-8", "{chemin}");
            assert_eq!(corps, MODULE, "{chemin}");
        }
        // Ressource absente : 404, pas index.html (un module HTML serait
        // refuse sans bruit par le navigateur).
        let (statut, _, _) = obtenir(&format!("{base}/assets/absent.js")).await;
        assert_eq!(statut, 404);
        // Sortie du repertoire : refusee.
        let (statut, _, _) = obtenir(&format!("{base}/assets/..%2Findex.html")).await;
        assert_eq!(statut, 404, "/assets/ ne doit servir que dist/assets");
    }

    /// Sans `TUNE_BRIDGE_WEB_DIR`, rien n'est monte : `/oauth/retour` est un
    /// 404 comme le reste du client, et les routes du relais sont intactes.
    #[tokio::test]
    async fn sans_client_web_le_retour_oauth_reste_un_404() {
        let base = pont_avec_client(None).await;
        let (statut, _, _) = obtenir(&format!("{base}/oauth/retour?code=abc&state=xyz")).await;
        assert_eq!(statut, 404);
        let (statut, _, _) = obtenir(&format!("{base}/assets/index-abc.js")).await;
        assert_eq!(statut, 404);
        let (statut, _, _) = obtenir(&format!("{base}/health")).await;
        assert_eq!(statut, 200);
    }

    /// Le retour fixe n'ouvre pas `/oauth/*` en grand, et ne rend pas le
    /// relais permissif : les chemins non UUID restent des 404.
    #[tokio::test]
    async fn le_retour_oauth_n_avale_pas_les_autres_chemins() {
        let client = client_construit();
        let base = pont_avec_client(Some(client.path())).await;
        for chemin in [
            "/oauth/autre",
            "/oauth/",
            "/oauth",
            "/pas-un-uuid/",
            "/health/x",
        ] {
            let (statut, _, _) = obtenir(&format!("{base}{chemin}")).await;
            assert_eq!(statut, 404, "{chemin} doit rester un 404");
        }
        let (statut, _, corps) = obtenir(&format!("{base}/{SERVEUR}/")).await;
        assert_eq!((statut, corps.as_str()), (200, INDEX));
        let (statut, _, _) = obtenir(&format!("{base}/health")).await;
        assert_eq!(statut, 200);
    }

    /// L'index et les fichiers du client portent la politique de contenu.
    /// `servir` est appele directement : les gestionnaires lisent
    /// `TUNE_BRIDGE_WEB_DIR`, que les tests voisins modifient.
    #[tokio::test]
    async fn l_index_et_les_fichiers_du_client_portent_la_politique_de_contenu() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("index.html"),
            "<!doctype html><title>Tune</title><script>var t=1;</script>\
             <script type=module src=/assets/index-abc.js></script>",
        )
        .unwrap();
        std::fs::write(tmp.path().join("index-abc.js"), "console.log('tune')").unwrap();
        for fichier in ["index.html", "index-abc.js"] {
            let r = servir(&tmp.path().join(fichier)).await;
            assert_eq!(r.status(), StatusCode::OK, "{fichier}");
            let h = r.headers();
            let csp = h
                .get("content-security-policy")
                .and_then(|v| v.to_str().ok())
                .unwrap_or_else(|| panic!("{fichier} : pas de Content-Security-Policy"));
            for directive in [
                "default-src 'self'",
                "script-src 'self'",
                "object-src 'none'",
                "frame-ancestors 'none'",
                "base-uri 'none'",
                "connect-src 'self' https://mozaiklabs.fr wss://bridge.mozaiklabs.fr",
            ] {
                assert!(
                    csp.contains(directive),
                    "{fichier} : {directive} absent de {csp}"
                );
            }
            assert!(!csp.contains("'unsafe-eval'"), "{fichier} : {csp}");
            let scripts = csp
                .split(';')
                .find(|d| d.trim_start().starts_with("script-src"))
                .unwrap();
            assert!(
                !scripts.contains("'unsafe-inline'"),
                "{fichier} : {scripts}"
            );
            assert_eq!(
                h.get("x-content-type-options")
                    .and_then(|v| v.to_str().ok()),
                Some("nosniff"),
                "{fichier}"
            );
            assert_eq!(
                h.get("referrer-policy").and_then(|v| v.to_str().ok()),
                Some("no-referrer"),
                "{fichier}"
            );
            // Seul l'index porte l'empreinte de son script en ligne.
            assert_eq!(
                csp.contains(&format!("'sha256-{}'", empreinte("var t=1;"))),
                fichier == "index.html",
                "{fichier} : {csp}"
            );
        }
    }

    /// Les scripts en ligne de l'index reel (theme, service worker) et son
    /// gestionnaire `onload` des polices sont autorises par empreinte, et
    /// eux seuls.
    #[test]
    fn les_scripts_en_ligne_de_l_index_sont_autorises_par_empreinte() {
        let index = "<!DOCTYPE html><html><head>\n\
            <!-- commentaire <script>non</script> -->\n\
            <link rel=\"stylesheet\" href=\"https://fonts.googleapis.com/css2\" media=\"print\" onload=\"this.media='all'\" />\n\
            </head><body>\n<script>\n  var t=1;\n</script>\n\
            <script type=\"module\" crossorigin src=\"/assets/index-abc.js\"></script>\n\
            <SCRIPT>b()</SCRIPT></body></html>";
        let (scripts, gestionnaires) = scripts_en_ligne(index);
        assert_eq!(
            scripts,
            vec!["\n  var t=1;\n".to_string(), "b()".to_string()]
        );
        assert_eq!(gestionnaires, vec!["this.media='all'".to_string()]);
        let csp = politique_de_contenu(Some(index));
        assert!(csp.contains("'unsafe-hashes'"), "{csp}");
        // Empreinte connue de `this.media='all'`.
        assert!(
            csp.contains("'sha256-MhtPZXr7+LpJUY5qtMutB+qWfQtMaPccfe7QXtCcEYc='"),
            "{csp}"
        );
        assert_eq!(csp.matches("'sha256-").count(), 3, "{csp}");
        // Sans HTML, aucune empreinte.
        assert!(!politique_de_contenu(None).contains("sha256"));
    }
}
