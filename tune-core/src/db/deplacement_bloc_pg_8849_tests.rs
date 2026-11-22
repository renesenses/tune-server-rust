use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pg_8849_deux_clients_ne_deplacent_pas_un_bloc_depuis_le_meme_ordre_perime() {
    let Ok(url) = std::env::var("TUNE_TEST_PG_URL") else {
        eprintln!("SAUT : TUNE_TEST_PG_URL non posée");
        return;
    };
    let pg = crate::db::postgres::PostgresDb::connect(&url)
        .await
        .unwrap();
    crate::db::migrations::run_pg_migrations(pg.pool())
        .await
        .unwrap();
    pg.ensure_schema().await;
    let backend: Arc<dyn DbBackend> =
        Arc::new(crate::db::backend::PostgresBackend::new(pg.pool().clone()));
    let zones = crate::db::zone_repo::ZoneRepo::with_backend(backend.clone());
    let zone = zones
        .create("Déplacement concurrent 8849", Some("browser"), None)
        .unwrap();
    let file = PlayQueueRepo::with_backend(backend.clone());
    let inputs: Vec<QueueInput> = (0..6)
        .map(|n| {
            serde_json::from_value(serde_json::json!({"Streaming": {
                "source":"qobuz", "source_id":format!("fixture-{n}"), "title":format!("Piste {n}"),
                "artist":"Banc", "album":null, "cover_url":null, "duration_ms":1000,
                "track_number":null,"disc_number":null,"album_ref":null,"artist_ref":null
            }}))
            .unwrap()
        })
        .collect();
    file.append(zone, &inputs).unwrap();
    file.set_current_pos(zone, 2).unwrap();
    let avant: Vec<_> = file
        .get_ordered(zone)
        .unwrap()
        .iter()
        .map(|e| e.id)
        .collect();
    let depart = Arc::new(tokio::sync::Barrier::new(2));
    let mut clients = Vec::new();
    for _ in 0..2 {
        let backend = backend.clone();
        let avant = avant.clone();
        let depart = depart.clone();
        clients.push(tokio::spawn(async move {
            depart.wait().await;
            PlayQueueRepo::with_backend(backend).deplacer_bloc(zone, 1, 3, 2, Some(&avant))
        }));
    }
    let mut valides = 0;
    let mut perimes = 0;
    for client in clients {
        match client.await.unwrap() {
            Ok(_) => valides += 1,
            Err(ErreurDeplacementBloc::OrdrePerime) => perimes += 1,
            autre => panic!("#8849 : refus inattendu {autre:?}"),
        }
    }
    assert_eq!(
        (valides, perimes),
        (1, 1),
        "#8849 : deux clients ont écrit depuis le même ordre"
    );
    let apres = file.get_ordered(zone).unwrap();
    assert_eq!(
        apres.iter().map(|e| e.id).collect::<Vec<_>>(),
        [0, 3, 4, 1, 2, 5]
            .iter()
            .map(|i| avant[*i])
            .collect::<Vec<_>>()
    );
    assert_eq!(apres.iter().find(|e| e.is_current).unwrap().id, avant[2]);
    backend
        .execute("DELETE FROM zones WHERE id = $1", &[&zone])
        .unwrap();
}
