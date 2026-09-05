use crate::state::AppState;
use tune_core::db::zone_repo::ZoneRepo;

/// Et le VERROU de branchement, sans lequel le test ci-dessous ne prouve
/// rien : il valide `build_zone_json`, pas le fait que les retours
/// anticipés s'en servent. Rebrancher un `to_value(z)` le laisserait vert.
///
/// C'est le même écart que JP a relevé quatre fois cette nuit — tester que
/// la fonction marche, pas qu'on l'appelle.
#[test]
fn les_retours_anticipes_passent_par_le_contrat() {
    // `create_zone` vit dans le module enfant `ecriture` depuis REF-4 (#2219).
    let src = std::fs::read_to_string(std::path::Path::new("src/routes/zones/ecriture.rs"))
        .expect("zones/ecriture.rs doit être lisible depuis la racine du crate");
    let debut = src
        .find("async fn create_zone(")
        .expect("create_zone doit exister");
    // La fonction suivante, quelle que soit sa visibilité : les items sortis
    // sont `pub(super)`, et une fenêtre ouverte jusqu'à la fin du fichier ne
    // mesurerait plus rien.
    let fin = [
        "\nasync fn ",
        "\npub async fn ",
        "\npub(super) async fn ",
        "\npub(crate) async fn ",
        "\nfn ",
        "\npub fn ",
        "\npub(super) fn ",
        "\npub(crate) fn ",
    ]
    .iter()
    .filter_map(|m| src[debut + 1..].find(m))
    .min()
    .map(|i| debut + 1 + i)
    .expect("une fonction doit suivre create_zone — sinon ce test ne borne plus rien");
    let corps = &src[debut..fin];

    assert_eq!(
        corps.matches("build_zone_json(").count(),
        3,
        "les TROIS retours anticipés doivent passer par le contrat client : \
         zone déjà associée au device, zone du même hôte sous une autre \
         identité SSDP (#1281), et rattrapage après collision UNIQUE"
    );
    assert!(
        !corps.contains("serde_json::to_value(z)"),
        "un retour anticipé sérialise encore la ligne brute : volume 50 au \
         lieu de 0.5, et les six champs d'état absents (#2284)"
    );
}

/// La contre-épreuve de JP Robbe sur #2284 : une zone qui existe déjà doit
/// ressortir dans le CONTRAT client, pas dans la forme brute de la base.
///
/// Les deux retours anticipés de `POST /zones` — zone déjà associée au
/// `output_device_id`, et rattrapage après collision `UNIQUE` — faisaient un
/// `serde_json::to_value(&zone)` : `volume: 50` au lieu de `0.5`, et les six
/// champs d'état absents. Le client ajoute cet objet à son magasin, donc le
/// curseur repartait au maximum malgré #2278.
#[tokio::test]
async fn une_zone_existante_ressort_au_contrat_client() {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let repo = ZoneRepo::with_backend(state.backend.clone());
    let id = repo
        .create("Salon", Some("dlna"), Some("uuid:abcd"))
        .unwrap();
    repo.update_volume(id, 50.0).unwrap();

    let v = crate::routes::playback::build_zone_json(&state, id).await;

    assert_eq!(
        v.get("volume").and_then(|x| x.as_f64()),
        Some(0.5),
        "volume en contrat client (0..1), pas la valeur de la base"
    );
    for champ in [
        "state",
        "current_track",
        "position_ms",
        "queue_length",
        "can_skip_next",
    ] {
        assert!(
            v.get(champ).is_some(),
            "{champ} absent : le client garderait la valeur d'une autre zone"
        );
    }
}

/// #2055 / #2092 — la charge utile rendue par les routes de LECTURE doit
/// dire l'aléatoire et la répétition du moteur, pas les taire.
///
/// Tades, quatre messages le 20/08 : « lecture aléatoire non demandée de
/// l'album », « quand j'appuie sur suivant, il choisit une piste au
/// hasard », « je ne pense pas avoir paramétré cela ». Le correctif #2153 a
/// rendu ces deux champs aux charges utiles de `zones.rs` ; celle de
/// `build_zone_json` — `play`, `pause`, `resume`, `stop`, `queue/jump`,
/// `pins/{i}/invoke` — ne les portait toujours pas, alors qu'elle porte déjà
/// `can_skip_next`, la décision qui DÉPEND de l'aléatoire (#2337).
///
/// La présence seule ne prouve rien : deux constantes en dur passeraient.
/// On arme donc le moteur et on exige que la charge utile le répète.
#[tokio::test]
async fn le_contrat_de_lecture_dit_l_aleatoire_et_la_repetition_du_moteur() {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let repo = ZoneRepo::with_backend(state.backend.clone());
    let id = repo
        .create("Salon", Some("dlna"), Some("uuid:abcd"))
        .unwrap();

    // Zone au repos : les deux réglages sortent à leur valeur de départ,
    // et pas en `null` ni en champ absent.
    let v = crate::routes::playback::build_zone_json(&state, id).await;
    assert_eq!(
        v.get("shuffle"),
        Some(&serde_json::json!(false)),
        "l'aléatoire est absent de la charge utile de lecture : le client \
         naîtrait de nouveau à « éteint » sans moyen d'apprendre le \
         contraire (#2092)"
    );
    assert_eq!(
        v.get("repeat"),
        Some(&serde_json::json!("off")),
        "la répétition est absente de la charge utile de lecture"
    );

    // Moteur armé — la file compte, sinon `set_shuffle` ne fabrique aucune
    // permutation et `can_skip_next` resterait faux pour une autre raison.
    state.playback.update_queue_info(id, 0, 5).await;
    state
        .playback
        .set_repeat(id, tune_core::playback::RepeatMode::All)
        .await;
    state.playback.set_shuffle(id, true).await;

    let v = crate::routes::playback::build_zone_json(&state, id).await;
    assert_eq!(
        v.get("shuffle"),
        Some(&serde_json::json!(true)),
        "le moteur tire au sort et la charge utile dit « non » : c'est \
         exactement l'écart vécu par Tades (#2055)"
    );
    assert_eq!(
        v.get("repeat"),
        Some(&serde_json::json!("all")),
        "`repeat` doit sortir en variante sérialisée (« all »), comme dans \
         `zones.rs` et sur le WebSocket — pas en « All » ni en nombre"
    );
}

/// #1281 — buchardt A700 : un appareil annoncé sous DEUX identités SSDP
/// (deux UUID, même hôte) apparaît deux fois dans le sélecteur. « I tried
/// creating a zone and it duplicates the zone output » : POST /zones avec
/// l'identité jumelle ne dédoublonnait que par `output_device_id` exact et
/// créait une deuxième zone pour le même renderer physique. Le regroupement
/// par hôte de la découverte doit s'appliquer ici aussi : la zone existante
/// est rendue (200), rien n'est créé.
#[tokio::test]
async fn poster_la_seconde_identite_ssdp_rend_la_zone_existante() {
    use axum::response::IntoResponse;

    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let repo = ZoneRepo::with_backend(state.backend.clone());
    let zid = repo
        .create("buchardt A700", Some("dlna"), Some("uuid:a700-dlna"))
        .unwrap();
    // L'identité physique que la découverte persiste (#942/#1239).
    repo.set_host(zid, "192.168.1.50").unwrap();

    // La jumelle du même appareil, déjà enregistrée comme sortie par la
    // découverte : même hôte, autre UUID.
    {
        let mut reg = state.outputs.lock().await;
        reg.register(Box::new(tune_core::outputs::dlna::DlnaOutput::new(
            "buchardt A700".into(),
            "uuid:a700-oh".into(),
            "192.168.1.50".into(),
            "http://192.168.1.50:49152/av".into(),
            "http://192.168.1.50:49152/rc".into(),
            None,
        )));
    }

    let resp = super::create_zone(
        axum::extract::State(state.clone()),
        axum::Json(super::CreateZone {
            name: "buchardt A700".into(),
            output_type: Some("dlna".into()),
            output_device_id: Some("uuid:a700-oh".into()),
        }),
    )
    .await
    .into_response();

    assert_eq!(
        resp.status(),
        axum::http::StatusCode::OK,
        "la zone existante du même hôte est rendue, pas créée (201)"
    );
    assert_eq!(
        repo.list().unwrap().len(),
        1,
        "toujours une seule zone pour l'appareil physique"
    );
}
