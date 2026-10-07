//! « Localiser sur le disque » ouvre le dossier de l'album dans le gestionnaire
//! de fichiers du système — tune-web-client#1875 (fil forum 2104).
//!
//! > « Serait-il possible de faire en sorte qu'en appuyant sur le bouton
//! > 'localiser sur le disque', sur Windows, l'explorateur de fichier s'ouvre
//! > directement dans le répertoire recherché ? »
//!
//! Un navigateur ne peut pas lancer l'Explorateur : seul le SERVEUR le peut, et
//! il l'ouvre sur SA machine. Ce n'est juste que si le navigateur tourne sur
//! cette même machine. D'où les trois verrous de cette route, tous tenus ici :
//!
//! 1. **Admin seulement** (`RequireAdmin`) : lancer un processus sur l'hôte
//!    n'est pas un geste d'écoute.
//! 2. **Le chemin vient de la BASE, jamais du client.** On ne reçoit qu'un
//!    identifiant d'album ; le dossier est celui de ses pistes locales, et il
//!    doit être absolu, sans `..`, sous une racine de la bibliothèque, et
//!    exister. Aucune chaîne fournie par l'appelant n'atteint la commande.
//! 3. **Jamais pour une machine distante.** L'appel doit venir de CETTE
//!    machine : pair de la socket en boucle locale, en-tête `Host` en boucle
//!    locale, et aucune trace de mandataire. Le relais nuage
//!    (`tune_core::cloud::relay`) rejoue les requêtes distantes depuis
//!    127.0.0.1 : il les marque d'un en-tête [`ENTETE_RELAIS`], qu'un appelant
//!    distant peut ajouter mais jamais retirer. Une requête marquée est
//!    refusée.
//!
//! `GET /library/reveal/available` répond la même question sans rien lancer :
//! le client n'affiche le bouton que lorsqu'il servirait.

use std::net::{IpAddr, SocketAddr};

use axum::Json;
use axum::extract::{ConnectInfo, FromRequestParts, Path, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde_json::json;
use tune_core::db::backend::ToSqlValue;
use tune_core::db::engine::{Engine, PostgresDialect, SqlDialect, SqliteDialect};
use tune_http_types::panne_sql::OuDefautJournalise;

use crate::auth::RequireAdmin;
use crate::state::AppState;

/// Posé par le relais nuage sur chaque requête qu'il rejoue en local.
use tune_core::cloud::ENTETE_RELAIS;

/// Les en-têtes qui disent qu'un mandataire s'est interposé : le pair de la
/// socket n'est alors plus l'appelant.
const ENTETES_MANDATAIRE: &[&str] = &[
    ENTETE_RELAIS,
    "x-bridge-token",
    "x-forwarded-for",
    "x-forwarded-host",
    "forwarded",
    "x-real-ip",
];

/// L'appelant tel que la socket et les en-têtes le décrivent.
pub(crate) struct Appelant {
    pair: Option<IpAddr>,
    entetes: HeaderMap,
}

impl<S: Send + Sync> FromRequestParts<S> for Appelant {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Self::Rejection> {
        Ok(Self {
            pair: parts
                .extensions
                .get::<ConnectInfo<SocketAddr>>()
                .map(|ConnectInfo(a)| a.ip()),
            entetes: parts.headers.clone(),
        })
    }
}

fn boucle_locale(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_loopback(),
        IpAddr::V6(v6) => {
            v6.is_loopback() || v6.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback())
        }
    }
}

/// `localhost`, `127.x.y.z` ou `[::1]`, avec ou sans port.
fn hote_en_boucle_locale(hote: &str) -> bool {
    let h = hote.trim().to_ascii_lowercase();
    let nom = if let Some(reste) = h.strip_prefix('[') {
        reste.split(']').next().unwrap_or("")
    } else {
        h.rsplit_once(':').map_or(h.as_str(), |(n, port)| {
            if port.chars().all(|c| c.is_ascii_digit()) {
                n
            } else {
                h.as_str()
            }
        })
    };
    nom == "localhost" || nom.parse::<IpAddr>().is_ok_and(boucle_locale)
}

/// L'appel vient-il de CETTE machine ? `Err` porte la raison, stable, que le
/// client peut lire.
pub(crate) fn appel_de_cette_machine(
    pair: Option<IpAddr>,
    entetes: &HeaderMap,
) -> Result<(), &'static str> {
    let Some(pair) = pair else {
        // Une couche qui n'aurait pas posé l'adresse doit FERMER, pas ouvrir.
        return Err("reveal_unknown_client");
    };
    if !boucle_locale(pair) {
        return Err("reveal_remote_client");
    }
    if ENTETES_MANDATAIRE.iter().any(|e| entetes.contains_key(*e)) {
        return Err("reveal_proxied");
    }
    let hote = entetes
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !hote_en_boucle_locale(hote) {
        return Err("reveal_remote_host");
    }
    Ok(())
}

fn dossier_du_fichier(chemin: &str) -> Option<&str> {
    let i = chemin.rfind(['/', '\\'])?;
    let d = &chemin[..i];
    (!d.is_empty()).then_some(d)
}

/// Le dossier COMMUN des pistes d'un album — la règle du bouton côté client
/// (`dossierDeLAlbum`, `lib/dossierAlbum.ts`) : un coffret CD1/CD2 ouvre son
/// dossier parent. `None` si les pistes ne partagent rien de plus que la racine.
pub(crate) fn dossier_commun(chemins: &[String]) -> Option<String> {
    let dossiers: Vec<&str> = chemins
        .iter()
        .filter_map(|c| dossier_du_fichier(c))
        .collect();
    let premier = *dossiers.first()?;
    let sep = if premier.contains('\\') && !premier.contains('/') {
        '\\'
    } else {
        '/'
    };
    let segments: Vec<Vec<&str>> = dossiers.iter().map(|d| d.split(sep).collect()).collect();
    let mut commun: Vec<&str> = Vec::new();
    for (i, s) in segments[0].iter().enumerate() {
        if segments.iter().all(|x| x.get(i) == Some(s)) {
            commun.push(s);
        } else {
            break;
        }
    }
    if commun.iter().all(|s| s.is_empty()) {
        return None;
    }
    Some(commun.join(&sep.to_string()))
}

/// Le dossier est-il ouvrable sans risque ? Absolu, sans `..`, sous une
/// racine de la bibliothèque. L'existence est vérifiée à part (disque).
pub(crate) fn dossier_admissible(dossier: &str, racines: &[String]) -> Result<(), &'static str> {
    if dossier.split(['/', '\\']).any(|c| c == "..") {
        return Err("invalid_path");
    }
    let absolu = dossier.starts_with('/')
        || dossier.starts_with("\\\\")
        || (dossier.len() >= 3
            && dossier.as_bytes()[1] == b':'
            && matches!(dossier.as_bytes()[2], b'\\' | b'/'));
    if !absolu {
        return Err("invalid_path");
    }
    let d = tune_core::scanner::walker::normalize_path(dossier);
    let sous_une_racine = racines
        .iter()
        .map(|r| tune_core::scanner::walker::normalize_path(r))
        .filter(|r| !r.is_empty())
        .any(|r| tune_core::metadata::enrich_scope::sous_le_dossier(&d, &r));
    if !sous_une_racine {
        return Err("path_outside_music_dirs");
    }
    Ok(())
}

/// La commande qui ouvre un dossier dans le gestionnaire de fichiers. Le
/// dossier est passé comme UN argument, jamais à travers un interpréteur.
fn commande_d_ouverture(dossier: &str) -> Option<std::process::Command> {
    if cfg!(target_os = "windows") {
        let mut c = std::process::Command::new("explorer.exe");
        c.arg(dossier);
        Some(c)
    } else if cfg!(target_os = "macos") {
        let mut c = std::process::Command::new("open");
        c.arg(dossier);
        Some(c)
    } else if cfg!(target_os = "linux") {
        let mut c = std::process::Command::new("xdg-open");
        c.arg(dossier);
        Some(c)
    } else {
        None
    }
}

fn refus(statut: StatusCode, code: &'static str) -> Response {
    (statut, Json(json!({ "error": code }))).into_response()
}

/// `GET /library/reveal/available` — le bouton servirait-il ici ?
pub(super) async fn revelation_disponible(
    _admin: RequireAdmin,
    appelant: Appelant,
) -> Json<serde_json::Value> {
    let plateforme = commande_d_ouverture("/").is_some();
    match appel_de_cette_machine(appelant.pair, &appelant.entetes) {
        Ok(()) if plateforme => Json(json!({ "available": true })),
        Ok(()) => Json(json!({ "available": false, "reason": "reveal_unsupported_platform" })),
        Err(raison) => Json(json!({ "available": false, "reason": raison })),
    }
}

fn chemins_de_l_album(state: &AppState, album_id: i64) -> Vec<String> {
    let p1 = match state.backend.engine() {
        Engine::Postgres => PostgresDialect.placeholder(1),
        Engine::Sqlite => SqliteDialect.placeholder(1),
    };
    state
        .backend
        .query_many(
            &format!(
                "SELECT file_path FROM tracks WHERE album_id = {p1} AND file_path IS NOT NULL \
                 AND (source IS NULL OR source = 'local')"
            ),
            &[&album_id as &dyn ToSqlValue],
        )
        .ou_defaut_journalise()
        .into_iter()
        .filter_map(|r| r.first().and_then(|v| v.as_string()))
        .collect()
}

/// `POST /library/albums/{id}/reveal` — ouvre le dossier de l'album dans le
/// gestionnaire de fichiers de CETTE machine.
///
/// - 200 `{"status":"opened","path":…}` ;
/// - 403 `reveal_*` : l'appel ne vient pas de cette machine ;
/// - 404 `album_folder_not_found` : aucune piste locale, ou dossier absent du disque ;
/// - 400 `invalid_path` / `path_outside_music_dirs` ;
/// - 501 `reveal_unsupported_platform` ; 500 `reveal_failed`.
pub(super) async fn reveler_album(
    State(state): State<AppState>,
    _admin: RequireAdmin,
    appelant: Appelant,
    Path(album_id): Path<i64>,
) -> Response {
    if let Err(raison) = appel_de_cette_machine(appelant.pair, &appelant.entetes) {
        return refus(StatusCode::FORBIDDEN, raison);
    }
    let Some(dossier) = dossier_commun(&chemins_de_l_album(&state, album_id)) else {
        return refus(StatusCode::NOT_FOUND, "album_folder_not_found");
    };
    let racines = crate::routes::system::get_music_dirs_list(&state.backend);
    if let Err(code) = dossier_admissible(&dossier, &racines) {
        return refus(StatusCode::BAD_REQUEST, code);
    }
    if !std::path::Path::new(&dossier).is_dir() {
        return refus(StatusCode::NOT_FOUND, "album_folder_not_found");
    }
    let Some(mut commande) = commande_d_ouverture(&dossier) else {
        return refus(StatusCode::NOT_IMPLEMENTED, "reveal_unsupported_platform");
    };
    match commande.spawn() {
        Ok(mut enfant) => {
            // Récolter le processus, sans faire attendre la réponse : un
            // `xdg-open` jamais attendu resterait zombie.
            std::thread::spawn(move || {
                let _ = enfant.wait();
            });
            tracing::info!(album_id, "album_folder_revealed");
            Json(json!({ "status": "opened", "path": dossier })).into_response()
        }
        Err(e) => {
            tracing::warn!(album_id, error = %e, "album_folder_reveal_failed");
            refus(StatusCode::INTERNAL_SERVER_ERROR, "reveal_failed")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;
    use std::net::{Ipv4Addr, Ipv6Addr};

    fn entetes(paires: &[(&'static str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in paires {
            h.insert(*k, HeaderValue::from_str(v).unwrap());
        }
        h
    }
    const LOCAL: Option<IpAddr> = Some(IpAddr::V4(Ipv4Addr::LOCALHOST));

    #[test]
    fn le_navigateur_de_cette_machine_est_admis() {
        assert_eq!(
            appel_de_cette_machine(LOCAL, &entetes(&[("host", "localhost:8888")])),
            Ok(())
        );
        assert_eq!(
            appel_de_cette_machine(LOCAL, &entetes(&[("host", "127.0.0.1:8888")])),
            Ok(())
        );
        let v6 = Some(IpAddr::V6(Ipv6Addr::LOCALHOST));
        assert_eq!(
            appel_de_cette_machine(v6, &entetes(&[("host", "[::1]:8888")])),
            Ok(())
        );
    }

    #[test]
    fn un_poste_du_reseau_local_est_refuse() {
        let lan = Some(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 20)));
        assert_eq!(
            appel_de_cette_machine(lan, &entetes(&[("host", "192.168.1.10:8888")])),
            Err("reveal_remote_client")
        );
    }

    #[test]
    fn sans_adresse_connue_la_reponse_est_non() {
        assert_eq!(
            appel_de_cette_machine(None, &entetes(&[("host", "localhost")])),
            Err("reveal_unknown_client")
        );
    }

    #[test]
    fn une_requete_rejouee_par_le_relais_est_refusee() {
        // Le relais rejoue depuis 127.0.0.1, `Host: 127.0.0.1:<port>` : seul
        // son marqueur le distingue du navigateur local.
        let h = entetes(&[("host", "127.0.0.1:8888"), (ENTETE_RELAIS, "1")]);
        assert_eq!(appel_de_cette_machine(LOCAL, &h), Err("reveal_proxied"));
    }

    #[test]
    fn un_mandataire_local_est_refuse() {
        for e in [
            "x-forwarded-for",
            "forwarded",
            "x-real-ip",
            "x-bridge-token",
            "x-forwarded-host",
        ] {
            let h = entetes(&[("host", "localhost:8888"), (e, "203.0.113.9")]);
            assert_eq!(
                appel_de_cette_machine(LOCAL, &h),
                Err("reveal_proxied"),
                "{e}"
            );
        }
    }

    #[test]
    fn un_hote_public_est_refuse_meme_en_boucle_locale() {
        let h = entetes(&[("host", "tune.exemple.test")]);
        assert_eq!(appel_de_cette_machine(LOCAL, &h), Err("reveal_remote_host"));
        assert_eq!(
            appel_de_cette_machine(LOCAL, &HeaderMap::new()),
            Err("reveal_remote_host")
        );
    }

    #[test]
    fn le_dossier_commun_suit_la_regle_du_client() {
        let un = vec![
            "/m/A/Album/01.flac".to_string(),
            "/m/A/Album/02.flac".to_string(),
        ];
        assert_eq!(dossier_commun(&un).as_deref(), Some("/m/A/Album"));
        let coffret = vec![
            "/m/A/Album/CD1/01.flac".to_string(),
            "/m/A/Album/CD2/01.flac".to_string(),
        ];
        assert_eq!(dossier_commun(&coffret).as_deref(), Some("/m/A/Album"));
        let win = vec![r"Z:\Musique\A\Album\01.flac".to_string()];
        assert_eq!(dossier_commun(&win).as_deref(), Some(r"Z:\Musique\A\Album"));
        assert_eq!(dossier_commun(&[]), None);
        let rien_en_commun = vec!["/a/x.flac".to_string(), "/b/y.flac".to_string()];
        assert_eq!(dossier_commun(&rien_en_commun), None);
    }

    #[test]
    fn le_dossier_doit_etre_sous_une_racine() {
        let racines = vec!["/m".to_string(), r"Z:\Musique".to_string()];
        assert_eq!(dossier_admissible("/m/A/Album", &racines), Ok(()));
        assert_eq!(dossier_admissible(r"Z:\Musique\A", &racines), Ok(()));
        assert_eq!(
            dossier_admissible("/etc", &racines),
            Err("path_outside_music_dirs")
        );
        assert_eq!(
            dossier_admissible("/mx/A", &racines),
            Err("path_outside_music_dirs")
        );
        assert_eq!(
            dossier_admissible("/m/../etc", &racines),
            Err("invalid_path")
        );
        assert_eq!(dossier_admissible("m/A", &racines), Err("invalid_path"));
        assert_eq!(
            dossier_admissible("/m/A", &[]),
            Err("path_outside_music_dirs")
        );
    }

    // ── Par le routeur, comme un client ─────────────────────────────────────

    use axum::body::Body;
    use axum::http::Request;
    use serde_json::Value;
    use tower::ServiceExt;
    use tune_core::db::settings_repo::SettingsRepo;

    fn etat_avec_album(dossier: &str) -> AppState {
        let etat = AppState::new(":memory:", 0, Default::default()).expect("état en mémoire");
        let b = &etat.backend;
        b.execute(
            "INSERT INTO albums (id, title) VALUES (5, 'Kind of Blue')",
            &[],
        )
        .expect("album");
        let chemin = format!("{dossier}/01.flac");
        b.execute(
            "INSERT INTO tracks (title, album_id, file_path, source, track_number) \
             VALUES ('So What', 5, ?1, 'local', 1)",
            &[&chemin as &dyn ToSqlValue],
        )
        .expect("piste");
        etat
    }

    async fn reveler(
        etat: &AppState,
        pair: [u8; 4],
        entetes: &[(&str, &str)],
    ) -> (StatusCode, Value) {
        let mut r = Request::post("/albums/5/reveal");
        for (k, v) in entetes {
            r = r.header(*k, *v);
        }
        let mut requete = r.body(Body::empty()).expect("requête");
        requete
            .extensions_mut()
            .insert(ConnectInfo(SocketAddr::from((pair, 51000))));
        let reponse = super::super::router()
            .with_state(etat.clone())
            .oneshot(requete)
            .await
            .expect("réponse");
        let statut = reponse.status();
        let octets = axum::body::to_bytes(reponse.into_body(), usize::MAX)
            .await
            .expect("corps");
        (
            statut,
            serde_json::from_slice(&octets).unwrap_or(Value::Null),
        )
    }

    #[tokio::test]
    async fn la_route_refuse_un_poste_du_reseau() {
        let etat = etat_avec_album("/m/Miles/Kind of Blue");
        let (statut, corps) =
            reveler(&etat, [192, 168, 1, 20], &[("host", "192.168.1.10:8888")]).await;
        assert_eq!(statut, StatusCode::FORBIDDEN, "{corps}");
        assert_eq!(corps["error"], "reveal_remote_client");
    }

    #[tokio::test]
    async fn la_route_refuse_une_requete_relayee() {
        let etat = etat_avec_album("/m/Miles/Kind of Blue");
        let (statut, corps) = reveler(
            &etat,
            [127, 0, 0, 1],
            &[("host", "127.0.0.1:8888"), (ENTETE_RELAIS, "1")],
        )
        .await;
        assert_eq!(statut, StatusCode::FORBIDDEN, "{corps}");
        assert_eq!(corps["error"], "reveal_proxied");
    }

    #[tokio::test]
    async fn la_route_refuse_un_dossier_hors_bibliotheque() {
        let etat = etat_avec_album("/hors/Miles/Kind of Blue");
        SettingsRepo::with_backend(etat.backend.clone())
            .set("music_dirs", r#"["/m"]"#)
            .expect("racines");
        let (statut, corps) = reveler(&etat, [127, 0, 0, 1], &[("host", "localhost:8888")]).await;
        assert_eq!(statut, StatusCode::BAD_REQUEST, "{corps}");
        assert_eq!(corps["error"], "path_outside_music_dirs");
    }

    #[tokio::test]
    async fn la_route_ne_lance_rien_pour_un_dossier_absent_du_disque() {
        let etat = etat_avec_album("/m/inexistant-1875/Kind of Blue");
        SettingsRepo::with_backend(etat.backend.clone())
            .set("music_dirs", r#"["/m"]"#)
            .expect("racines");
        let (statut, corps) = reveler(&etat, [127, 0, 0, 1], &[("host", "localhost:8888")]).await;
        assert_eq!(statut, StatusCode::NOT_FOUND, "{corps}");
        assert_eq!(corps["error"], "album_folder_not_found");
    }

    #[tokio::test]
    async fn la_disponibilite_dit_non_a_un_poste_distant() {
        let etat = AppState::new(":memory:", 0, Default::default()).expect("état en mémoire");
        let mut requete = Request::get("/reveal/available")
            .header("host", "192.168.1.10:8888")
            .body(Body::empty())
            .expect("requête");
        requete
            .extensions_mut()
            .insert(ConnectInfo(SocketAddr::from(([192, 168, 1, 20], 51000))));
        let reponse = super::super::router()
            .with_state(etat)
            .oneshot(requete)
            .await
            .expect("réponse");
        assert_eq!(reponse.status(), StatusCode::OK);
        let octets = axum::body::to_bytes(reponse.into_body(), usize::MAX)
            .await
            .expect("corps");
        let corps: Value = serde_json::from_slice(&octets).expect("json");
        assert_eq!(corps["available"], false);
        assert_eq!(corps["reason"], "reveal_remote_client");
    }
}
