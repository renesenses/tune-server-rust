//! #3835 / #3838 — la zone « Volumio » qui **ne pointe sur rien**.
//!
//! JeromeQ, fil 1750 : une carte de zone nommée « Volumio » qui porte les
//! trois libellés d'un coup — surtitre « SORTIE LOCALE », badge rouge
//! « Aucune sortie — la lecture sera refusée », badge « Jamais vue depuis la
//! mise à jour » — pendant que le boîtier, lui, est découvert deux fois
//! (DLNA et OpenHome) à la même adresse et coché.
//!
//! Les deux issues décrivent **la même ligne de base** : une zone non
//! navigateur dont `output_device_id` est NULL. Ce n'est pas « l'appareil ne
//! répond pas », c'est « il n'y a rien à appeler ».
//!
//! ## Le chemin de naissance, lu — pas supposé
//!
//! `tune-web-client`, `src/components/Sidebar.svelte` (et
//! `ZoneManagerView.svelte`) : le formulaire « créer une zone » n'exige que le
//! **nom** (`if (!newZoneName.trim()) return;`), laisse `output_type` sur son
//! défaut `'local'` et n'affiche un sélecteur d'appareil que pour
//! `dlna | airplay | snapcast | sonos`. `newZoneDeviceId` part à `undefined`
//! et le reste. `api.createZone(nom, 'local', undefined)` poste donc
//! `{"name":"Volumio","output_type":"local","output_device_id":null}` — et
//! côté serveur **toutes** les vérifications d'appareil de `create_zone` sont
//! sous `if let Some(device_id) = output_device_id` : un corps sans appareil
//! les saute toutes et atteint l'`INSERT`.
//!
//! ## Pourquoi refuser n'est pas un arbitrage de produit
//!
//! La règle « une zone non navigateur sans appareil ne joue nulle part » était
//! déjà écrite **deux fois** et appliquée **après coup** :
//!
//! - `routes/playback.rs`, `reject_if_zone_has_no_output_device` : 409
//!   `zone_no_output_device` à la première lecture ;
//! - `routes/zones.rs`, `output_reach_of` : `no_output`, le badge rouge.
//!
//! Ces témoins ne font qu'ajouter le **troisième consommateur, à la
//! naissance** : `POST /zones` refuse ce que `POST /zones/{id}/play` refusera
//! de toute façon. La zone navigateur garde son exemption, écrite au même
//! endroit que les deux autres — [`zone_sans_appareil`].

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_core::db::zone_repo::ZoneRepo;

use super::zone_sans_appareil;

fn serveur() -> (axum::Router, crate::state::AppState) {
    let state = crate::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    let routeur = crate::routes::router(state.clone());
    (routeur, state)
}

/// **La ROUTE**, pas le gestionnaire : `POST /api/v1/zones` traversé par le
/// routeur complet, `ConnectInfo` compris — c'est lui que `create_zone_handler`
/// extrait pour suffixer le nom des zones navigateur.
async fn poster_une_zone(app: &axum::Router, corps: Value) -> (StatusCode, Value) {
    let mut requete = Request::builder()
        .method("POST")
        .uri("/api/v1/zones")
        .header("Content-Type", "application/json")
        .body(Body::from(corps.to_string()))
        .unwrap();
    requete
        .extensions_mut()
        .insert(ConnectInfo(std::net::SocketAddr::from((
            [127, 0, 0, 1],
            51835,
        ))));
    let reponse = app.clone().oneshot(requete).await.unwrap();
    let statut = reponse.status();
    let octets = axum::body::to_bytes(reponse.into_body(), 1 << 20)
        .await
        .unwrap();
    (
        statut,
        serde_json::from_slice(&octets).unwrap_or(Value::Null),
    )
}

/// La règle, seule. Trois consommateurs s'en servent — la création, le refus
/// de lecture et le badge — et c'est ce qui interdit qu'ils divergent.
#[test]
fn la_regle_exempte_le_navigateur_et_personne_d_autre() {
    assert!(
        zone_sans_appareil(Some("local"), None),
        "le corps exact du formulaire web : nom + output_type local, aucun \
         appareil — c'est la zone de JeromeQ"
    );
    assert!(
        zone_sans_appareil(None, None),
        "sans output_type non plus : le client web l'omet, `OUTPUTS[z.output_type \
         ?? 'local']` affiche « Sortie locale » dans les deux cas"
    );
    assert!(
        zone_sans_appareil(Some("openhome"), None),
        "aucun type réseau n'est jouable sans appareil"
    );
    assert!(
        !zone_sans_appareil(Some("browser"), None),
        "une zone navigateur n'a JAMAIS d'output_device_id : la sortie, c'est \
         l'onglet (`transport.rs`). C'est la seule exemption."
    );
    assert!(
        !zone_sans_appareil(Some("local"), Some("local:Haut-Parleurs")),
        "une zone qui pointe sur un appareil n'est pas concernée"
    );
}

/// Le ticket, mot pour mot : le corps que `Sidebar.svelte` envoie quand on
/// tape « Volumio » et qu'on valide sans toucher au sélecteur de type.
#[tokio::test]
async fn la_route_refuse_la_zone_du_ticket() {
    let (app, state) = serveur();

    let (statut, corps) =
        poster_une_zone(&app, json!({"name": "Volumio", "output_type": "local"})).await;

    assert_eq!(
        statut,
        StatusCode::BAD_REQUEST,
        "la route a accepté de créer une zone que la lecture refusera : \
         c'est #3835 / #3838. Corps rendu : {corps}"
    );
    assert!(
        ZoneRepo::with_backend(state.backend.clone())
            .list()
            .unwrap()
            .is_empty(),
        "refus annoncé mais ligne écrite : la zone orpheline survivrait au 400"
    );
}

/// Le même défaut sans `output_type` du tout — la colonne reste NULL, la carte
/// affiche « SORTIE LOCALE » par le `?? 'local'` du client. Rien ne doit
/// dépendre de la présence du champ.
#[tokio::test]
async fn la_route_refuse_aussi_quand_le_type_est_absent() {
    let (app, state) = serveur();

    let (statut, corps) = poster_une_zone(&app, json!({"name": "Volumio"})).await;

    assert_eq!(
        statut,
        StatusCode::BAD_REQUEST,
        "un corps sans output_type saute toutes les vérifications d'appareil. \
         Corps rendu : {corps}"
    );
    assert!(
        ZoneRepo::with_backend(state.backend.clone())
            .list()
            .unwrap()
            .is_empty()
    );
}

/// L'exemption, par la route. Si ce témoin rougit, le garde a cassé le client
/// web : chaque onglet crée sa zone `browser` **sans appareil**, par dessein.
#[tokio::test]
async fn la_zone_navigateur_naît_toujours_sans_appareil() {
    let (app, state) = serveur();

    let (statut, corps) = poster_une_zone(
        &app,
        json!({"name": "Cet ordinateur", "output_type": "browser"}),
    )
    .await;

    assert_eq!(
        statut,
        StatusCode::CREATED,
        "le garde a mordu sur la zone navigateur : plus aucun onglet ne peut \
         jouer. Corps rendu : {corps}"
    );
    let zones = ZoneRepo::with_backend(state.backend.clone())
        .list()
        .unwrap();
    assert_eq!(zones.len(), 1, "la zone navigateur doit exister");
    assert_eq!(zones[0].output_type.as_deref(), Some("browser"));
    assert!(
        zones[0].output_device_id.is_none(),
        "et elle n'a toujours pas d'appareil — c'est le contrat, pas un défaut"
    );
}

/// L'autre moitié : une zone qui PORTE un appareil naît toujours. Sans ce
/// témoin, un garde trop large passerait inaperçu.
#[tokio::test]
async fn une_zone_avec_appareil_nait_toujours() {
    let (app, state) = serveur();

    let (statut, corps) = poster_une_zone(
        &app,
        json!({
            "name": "Volumio",
            "output_type": "openhome",
            "output_device_id": "uuid:04255cb4-11f0-58db-0053-3c7c3fcdbb5c"
        }),
    )
    .await;

    assert_eq!(
        statut,
        StatusCode::CREATED,
        "l'appareil n'est pas encore découvert dans ce banc, et ce n'est PAS \
         un motif de refus : la route journalise \
         `create_zone_device_not_discovered` et crée la zone. Corps : {corps}"
    );
    let zones = ZoneRepo::with_backend(state.backend.clone())
        .list()
        .unwrap();
    assert_eq!(
        zones[0].output_device_id.as_deref(),
        Some("uuid:04255cb4-11f0-58db-0053-3c7c3fcdbb5c")
    );
}

/// Ce que devient une zone orpheline **déjà en base** — celle de JeromeQ, née
/// avant ce garde. Le garde ne l'efface pas : il empêche la suivante. Ce
/// témoin fixe ce que le serveur en dit, et prouve que les deux consommateurs
/// historiques lisent bien la même règle que la création.
#[tokio::test]
async fn une_zone_orpheline_deja_en_base_reste_signalee_et_refusee() {
    let (app, state) = serveur();
    let repo = ZoneRepo::with_backend(state.backend.clone());
    let zone_id = repo.create("Volumio", Some("local"), None).unwrap();

    let zone = repo.get(zone_id).unwrap().unwrap();
    assert!(
        zone_sans_appareil(
            zone.output_type.as_deref(),
            zone.output_device_id.as_deref()
        ),
        "préalable du témoin : la ligne de #3835 est bien celle-là"
    );

    // Le badge rouge de la capture 1 — `GET /zones`.
    let requete = Request::builder()
        .method("GET")
        .uri("/api/v1/zones")
        .body(Body::empty())
        .unwrap();
    let reponse = app.clone().oneshot(requete).await.unwrap();
    assert_eq!(reponse.status(), StatusCode::OK);
    let octets = axum::body::to_bytes(reponse.into_body(), 1 << 22)
        .await
        .unwrap();
    let liste: Value = serde_json::from_slice(&octets).unwrap();
    let carte = liste
        .as_array()
        .and_then(|z| z.iter().find(|z| z["id"] == json!(zone_id)))
        .unwrap_or_else(|| panic!("la zone doit figurer dans GET /zones : {liste}"));
    assert_eq!(
        carte["output_reach"],
        json!("no_output"),
        "c'est le badge « Aucune sortie — la lecture sera refusée » : {carte}"
    );

    // Et la promesse du badge — `POST /zones/{id}/play`.
    let requete = Request::builder()
        .method("POST")
        .uri(format!("/api/v1/zones/{zone_id}/play"))
        .header("Content-Type", "application/json")
        .body(Body::from(json!({"track_ids": [1]}).to_string()))
        .unwrap();
    let reponse = app.clone().oneshot(requete).await.unwrap();
    let statut = reponse.status();
    let octets = axum::body::to_bytes(reponse.into_body(), 1 << 20)
        .await
        .unwrap();
    let corps = String::from_utf8_lossy(&octets).to_string();
    assert!(
        !statut.is_success(),
        "le badge promet un refus ; la route a répondu {statut} : {corps}"
    );
    assert!(
        corps.contains("zone_no_output_device"),
        "le refus doit rester nommé — c'est ce que les journaux d'un testeur \
         permettent de reconnaître. Reçu {statut} : {corps}"
    );
}

/// Le verrou de BRANCHEMENT. Sans lui, les témoins ci-dessus resteraient verts
/// si quelqu'un réécrivait la condition en clair dans `create_zone` — et la
/// règle se remettrait à diverger entre la création, la lecture et le badge,
/// ce qui EST le défaut de fond de #3835 / #3838.
///
/// L'aiguille est assemblée à l'exécution : écrite en clair, elle figurerait
/// dans ce fichier-ci, et un témoin qui se trouve lui-même ne mesure rien.
#[test]
fn les_trois_consommateurs_passent_par_la_meme_regle() {
    let regle = format!("{}(", "zone_sans_appareil");

    let ecriture = std::fs::read_to_string("src/routes/zones/ecriture.rs")
        .expect("zones/ecriture.rs doit être lisible depuis la racine du crate");
    let debut = ecriture
        .find("async fn create_zone(")
        .expect("create_zone doit exister");
    let corps = &ecriture[debut..];
    let fin = corps.find("\n/// Marque et modèle").expect(
        "le commentaire qui suit create_zone doit exister — sinon ce témoin ne borne plus rien",
    );
    let corps = &corps[..fin];
    assert!(
        corps.contains(&regle),
        "create_zone ne consulte plus la règle : une zone sans appareil peut \
         de nouveau naître, ou le refus a été réécrit en clair et divergera \
         (#3835 / #3838)"
    );

    let zones = std::fs::read_to_string("src/routes/zones.rs").expect("zones.rs doit être lisible");
    assert!(
        zones.matches(&regle).count() >= 2,
        "output_reach_of n'appelle plus la règle : le badge rouge et le refus \
         de création peuvent diverger"
    );

    let lecture =
        std::fs::read_to_string("src/routes/playback.rs").expect("playback.rs doit être lisible");
    assert!(
        lecture.contains(&regle),
        "reject_if_zone_has_no_output_device n'appelle plus la règle : le 409 \
         `zone_no_output_device` et le refus de création peuvent diverger"
    );
}
