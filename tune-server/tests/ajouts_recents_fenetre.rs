//! #3039 — « Ajouts récents » : la fenêtre est choisie, bornée, et honnête.
//!
//! Demandé par le testeur Sevy Tabroc (forum 1630) : « Possibilité de choisir
//! entre dans les derniers quinze jours et/ou dans le dernier mois ». La
//! fenêtre était écrite en dur à 7 jours dans `chrono_epoch_seven_days_ago()`,
//! une fonction SANS argument : aucun client ne pouvait en demander une autre.
//!
//! ## Pourquoi ce test passe par la ROUTE MONTÉE
//!
//! Un test qui reconstruirait la requête SQL à la main resterait VERT quand on
//! sabote le vrai code — c'est exactement ce qui s'est mesuré sur #3144. Ici
//! on monte `tune_server::routes::router(state)` et on lui envoie de vraies
//! requêtes HTTP sur `/api/v1/home/recently-added` : le paramètre doit donc
//! traverser l'`extractor`, le handler, la construction du SQL et le moteur.
//! Un handler qui ignorerait `days` fait tomber le test.
//!
//! ## Les quatre albums, et ce que chacun prouve
//!
//! | album      | `file_mtime` | `file_first_seen` | attendu à 7 j |
//! |------------|--------------|-------------------|---------------|
//! | Recent     | J-2          | —                 | présent       |
//! | Vieux      | J-60         | —                 | absent        |
//! | Restaure   | J-800        | J-3               | **présent**   |
//! | Recopie    | J-1          | J-200             | **absent**    |
//!
//! Les deux derniers sont la réserve du ticket rendue mesurable : `Restaure`
//! est une sauvegarde remise en place (le `mtime` est vieux, l'entrée en
//! bibliothèque est récente), `Recopie` est un `rsync -a` (le `mtime` est
//! frais, le fichier est là depuis des mois). Un filtre sur le seul `mtime`
//! se trompe sur les DEUX, en sens opposés.

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;
use tune_server::state::AppState;

/// Une base SQLite en mémoire, migrations appliquées comme au démarrage réel.
///
/// Pas de `TempDir` ni de fichier : `AppState::new(":memory:", …)` est le
/// chemin que les tests de `routes/home.rs` empruntent déjà, et il n'écrit
/// rien qu'un `/tmp` pourrait rendre vide.
fn etat() -> AppState {
    AppState::new(":memory:", 0, Default::default()).expect("AppState sur SQLite")
}

fn maintenant() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

fn il_y_a(jours: f64) -> f64 {
    maintenant() - jours * 24.0 * 3600.0
}

/// Insère un album d'une piste, avec le `mtime` voulu et, si demandé, une
/// première vue par le scan.
fn album(state: &AppState, titre: &str, mtime_jours: f64, premiere_vue_jours: Option<f64>) {
    let backend = &state.backend;
    backend
        .execute(
            &format!("INSERT INTO artists (name) VALUES ('Artiste {titre}')"),
            &[],
        )
        .expect("artiste");
    backend
        .execute(
            &format!(
                "INSERT INTO albums (title, artist_id, track_count) \
                 VALUES ('{titre}', (SELECT id FROM artists WHERE name = 'Artiste {titre}'), 1)"
            ),
            &[],
        )
        .expect("album");
    let chemin = format!("/musique/{titre}/01.flac");
    backend
        .execute(
            &format!(
                "INSERT INTO tracks (title, album_id, file_path, file_mtime, duration_ms) \
                 VALUES ('Piste de {titre}', \
                         (SELECT id FROM albums WHERE title = '{titre}'), \
                         '{chemin}', {}, 60000)",
                il_y_a(mtime_jours)
            ),
            &[],
        )
        .expect("piste");
    if let Some(j) = premiere_vue_jours {
        backend
            .execute(
                &format!(
                    "INSERT INTO file_first_seen (file_path, first_seen_at) \
                     VALUES ('{chemin}', {})",
                    il_y_a(j)
                ),
                &[],
            )
            .expect("première vue");
    }
}

/// La bibliothèque du tableau ci-dessus.
fn bibliotheque() -> AppState {
    let state = etat();
    album(&state, "Recent", 2.0, None);
    album(&state, "Vieux", 60.0, None);
    album(&state, "Restaure", 800.0, Some(3.0));
    album(&state, "Recopie", 1.0, Some(200.0));
    state
}

/// Une vraie requête HTTP sur le routeur monté. Rend le statut et le corps.
async fn appel(state: &AppState, chemin: &str) -> (StatusCode, Value) {
    let app: Router = tune_server::routes::router(state.clone());
    let reponse = app
        .oneshot(Request::get(chemin).body(Body::empty()).unwrap())
        .await
        .expect("le routeur doit répondre");
    let statut = reponse.status();
    let octets = axum::body::to_bytes(reponse.into_body(), 1024 * 1024)
        .await
        .expect("corps lisible");
    let corps = serde_json::from_slice(&octets).unwrap_or(Value::Null);
    (statut, corps)
}

/// Les titres rendus par la route, dans l'ordre.
fn titres(corps: &Value) -> Vec<String> {
    corps
        .as_array()
        .unwrap_or_else(|| panic!("la route doit rendre un TABLEAU d'albums, obtenu : {corps}"))
        .iter()
        .map(|a| a["title"].as_str().unwrap_or_default().to_string())
        .collect()
}

/// LE TÉMOIN — un appel SANS paramètre rend toujours les 7 jours d'avant.
///
/// C'est la moitié qui protège le parc : les clients déjà déployés n'envoient
/// pas `days`, et ne doivent rien voir changer. Le test vérifie les deux sens
/// — ce qui entre dans la fenêtre ET ce qui en reste dehors.
#[tokio::test(flavor = "multi_thread")]
async fn i3039_temoin_sans_parametre_la_fenetre_reste_de_sept_jours() {
    let state = bibliotheque();
    let (statut, corps) = appel(&state, "/api/v1/home/recently-added").await;
    assert_eq!(statut, StatusCode::OK, "corps : {corps}");
    let mut vus = titres(&corps);
    vus.sort();
    assert_eq!(
        vus,
        vec!["Recent".to_string(), "Restaure".to_string()],
        "sans `days`, la fenêtre doit valoir 7 jours — ni plus (Vieux à J-60 \
         doit rester dehors), ni moins. Obtenu : {corps}"
    );
}

/// La fenêtre demandée est SERVIE : 60 jours font entrer `Vieux`, que 7 jours
/// laissaient dehors. C'est la demande de Sevy Tabroc, littéralement.
#[tokio::test(flavor = "multi_thread")]
async fn i3039_la_fenetre_demandee_est_servie_quinze_trente_soixante_jours() {
    let state = bibliotheque();

    // 15 jours (« les derniers quinze jours ») : rien de plus que 7 ici, mais
    // la route doit l'accepter et rendre 200.
    let (statut, corps) = appel(&state, "/api/v1/home/recently-added?days=15").await;
    assert_eq!(statut, StatusCode::OK, "corps : {corps}");
    let mut a15 = titres(&corps);
    a15.sort();
    assert_eq!(a15, vec!["Recent".to_string(), "Restaure".to_string()]);

    // 60 jours : `Vieux` (J-60) est sur la borne, 61 le couvre à coup sûr.
    let (statut, corps) = appel(&state, "/api/v1/home/recently-added?days=61").await;
    assert_eq!(statut, StatusCode::OK, "corps : {corps}");
    let mut a61 = titres(&corps);
    a61.sort();
    assert_eq!(
        a61,
        vec![
            "Recent".to_string(),
            "Restaure".to_string(),
            "Vieux".to_string()
        ],
        "à 61 jours, `Vieux` (ajouté à J-60) doit entrer : sans cela le \
         paramètre n'est pas lu. Obtenu : {corps}"
    );

    // Contre-épreuve du même geste : une fenêtre PLUS ÉTROITE que le défaut
    // doit RETIRER des albums. Un handler qui ignore `days` rendrait les deux
    // mêmes listes et tomberait ici.
    let (statut, corps) = appel(&state, "/api/v1/home/recently-added?days=1").await;
    assert_eq!(statut, StatusCode::OK, "corps : {corps}");
    assert!(
        titres(&corps).is_empty(),
        "à 1 jour, aucun des quatre albums n'entre (le plus récent est à J-2) \
         — une liste non vide prouve que `days` est ignoré. Obtenu : {corps}"
    );
}

/// La fenêtre est BORNÉE, et l'erreur DIT la borne.
///
/// Ni écrêtage muet ni silence : un client qui demande 5 000 jours et reçoit
/// 730 jours de résultats croirait avoir tout.
#[tokio::test(flavor = "multi_thread")]
async fn i3039_la_fenetre_est_bornee_et_le_refus_dit_la_borne() {
    let state = bibliotheque();
    for (requete, pourquoi) in [
        ("?days=731", "au-dessus du plafond de 730 jours"),
        ("?days=0", "une fenêtre de zéro jour n'a pas de sens"),
        ("?days=-5", "une fenêtre négative n'a pas de sens"),
        ("?days=100000", "le balayage de toute la bibliothèque"),
    ] {
        let (statut, corps) = appel(&state, &format!("/api/v1/home/recently-added{requete}")).await;
        assert_eq!(
            statut,
            StatusCode::BAD_REQUEST,
            "`{requete}` doit être refusé ({pourquoi}), obtenu {statut} : {corps}"
        );
        let message = corps["error"].as_str().unwrap_or_default();
        assert!(
            message.contains("730"),
            "le refus doit DIRE la borne, pas seulement refuser. Message : {message}"
        );
    }
}

/// Le décompte du sous-titre : albums ET pistes ET durée, sur la même fenêtre.
///
/// « 7 albums • 71 pistes • 5 h 55 min » dans les captures du testeur. Le
/// décompte porte sur la FENÊTRE et non sur la page : `limit` ne le tronque
/// pas.
#[tokio::test(flavor = "multi_thread")]
async fn i3039_le_decompte_donne_albums_pistes_et_duree() {
    let state = bibliotheque();

    let (statut, corps) = appel(&state, "/api/v1/home/recently-added/summary").await;
    assert_eq!(statut, StatusCode::OK, "corps : {corps}");
    assert_eq!(corps["days"], 7, "la fenêtre par défaut : {corps}");
    assert_eq!(
        corps["album_count"], 2,
        "2 albums dans les 7 jours (Recent, Restaure) : {corps}"
    );
    assert_eq!(corps["track_count"], 2, "2 pistes : {corps}");
    assert_eq!(
        corps["duration_ms"], 120_000,
        "2 pistes d'une minute : {corps}"
    );
    assert_eq!(corps["duration_seconds"], 120, "{corps}");

    // La MÊME fenêtre que la liste : le décompte suit `days`.
    let (statut, corps) = appel(&state, "/api/v1/home/recently-added/summary?days=61").await;
    assert_eq!(statut, StatusCode::OK, "corps : {corps}");
    assert_eq!(corps["days"], 61, "{corps}");
    assert_eq!(
        corps["album_count"], 3,
        "à 61 jours, `Vieux` entre aussi — un décompte qui reste à 2 prouve \
         que `days` n'est pas lu par le sous-titre : {corps}"
    );
    assert_eq!(corps["track_count"], 3, "{corps}");
    assert_eq!(corps["duration_ms"], 180_000, "{corps}");

    // Le décompte est celui de la fenêtre, pas de la page : `limit=1` ne le
    // ramène pas à 1.
    let (statut, corps) = appel(
        &state,
        "/api/v1/home/recently-added/summary?days=61&limit=1",
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "corps : {corps}");
    assert_eq!(
        corps["album_count"], 3,
        "`limit` ne doit pas tronquer le décompte : {corps}"
    );

    // Et il est borné comme la liste.
    let (statut, _) = appel(&state, "/api/v1/home/recently-added/summary?days=731").await;
    assert_eq!(statut, StatusCode::BAD_REQUEST);
}

/// LA RÉSERVE DU TICKET, rendue mesurable : le `mtime` n'est pas la date
/// d'ajout.
///
/// `Restaure` a un `mtime` de plus de deux ans (sauvegarde remise en place) et
/// une première vue à J-3 : il DOIT apparaître. `Recopie` a un `mtime` d'hier
/// (`rsync -a`) et une première vue à J-200 : il ne DOIT PAS apparaître.
///
/// Un filtre sur le seul `mtime` — la forme d'avant #3039 — se trompe sur les
/// deux, en sens opposés. C'est le seul test du fichier qui tombe si l'on
/// retire la jointure `file_first_seen` sans toucher au reste.
#[tokio::test(flavor = "multi_thread")]
async fn i3039_la_date_dajout_prime_le_mtime_du_fichier() {
    let state = bibliotheque();
    let (statut, corps) = appel(&state, "/api/v1/home/recently-added").await;
    assert_eq!(statut, StatusCode::OK, "corps : {corps}");
    let vus = titres(&corps);

    assert!(
        vus.contains(&"Restaure".to_string()),
        "une sauvegarde remise en place hier (mtime à J-800, première vue à \
         J-3) doit apparaître dans les ajouts récents : c'est bien un ajout. \
         Obtenu : {corps}"
    );
    assert!(
        !vus.contains(&"Recopie".to_string()),
        "un `rsync -a` (mtime à J-1, première vue à J-200) NE DOIT PAS \
         apparaître : le fichier est là depuis des mois, seul son horodatage \
         a bougé. Obtenu : {corps}"
    );

    // Le repli reste exact pour les pistes scannées avant #473, qui n'ont
    // aucune ligne dans `file_first_seen` : `Recent` (mtime J-2, pas de
    // première vue) doit toujours être là.
    assert!(
        vus.contains(&"Recent".to_string()),
        "sans ligne dans `file_first_seen`, le `mtime` reste le repli — une \
         bibliothèque scannée avant #473 ne doit pas devenir vide. \
         Obtenu : {corps}"
    );

    // Et la date rendue est celle de l'AJOUT, pas celle du fichier.
    let restaure = corps
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["title"] == "Restaure")
        .expect("`Restaure` est dans la liste");
    let added = restaure["added_at"]
        .as_f64()
        .expect("added_at est un nombre");
    assert!(
        added > il_y_a(7.0),
        "`added_at` doit porter la première vue (J-3), pas le mtime (J-800) : \
         {restaure}"
    );
}

/// Le plancher du détecteur : la route est MONTÉE.
///
/// Treize cas d'« écrit mais pas branché » dans ce dépôt. Une route déclarée
/// dans `home::router()` mais dont le `nest` aurait bougé rendrait 404, et
/// tous les tests ci-dessus qui n'attendent « pas 500 » passeraient à vide.
#[tokio::test(flavor = "multi_thread")]
async fn i3039_les_deux_routes_sont_bien_montees() {
    let state = etat();
    for route in [
        "/api/v1/home/recently-added",
        "/api/v1/home/recently-added?days=30",
        "/api/v1/home/recently-added/summary",
        "/api/v1/home/recently-added/summary?days=30",
    ] {
        let (statut, corps) = appel(&state, route).await;
        assert_eq!(
            statut,
            StatusCode::OK,
            "`{route}` n'est pas atteinte par le routeur monté : un 404 ou un \
             401 prouverait que le handler n'a jamais été exécuté. Corps : {corps}"
        );
    }
}
