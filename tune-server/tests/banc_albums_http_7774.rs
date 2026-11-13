//! Banc manuel isolé : vraie route HTTP, bases synthétiques, aucun lecteur audio.
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use tune_core::db::{
    backend::{DbBackend, DbTxHandle, SqlValue, ToSqlValue},
    engine::Engine,
};
use tune_server::{config::TuneConfig, state::AppState};

struct MesureSql {
    base: Arc<dyn DbBackend>,
    actif: AtomicBool,
    lectures: Mutex<Vec<(String, Vec<SqlValue>, f64)>>,
}
impl MesureSql {
    fn retenir(&self, sql: &str, params: &[&dyn ToSqlValue], debut: Instant) {
        if self.actif.load(Ordering::Relaxed) {
            self.lectures.lock().unwrap().push((
                sql.into(),
                params.iter().map(|p| p.to_sql_value()).collect(),
                debut.elapsed().as_secs_f64() * 1000.,
            ));
        }
    }
}
impl DbBackend for MesureSql {
    fn engine(&self) -> Engine {
        self.base.engine()
    }
    fn jeton_des_donnees(&self) -> Option<(u64, i64)> {
        self.base.jeton_des_donnees()
    }
    fn execute(&self, s: &str, p: &[&dyn ToSqlValue]) -> Result<usize, String> {
        self.base.execute(s, p)
    }
    fn execute_batch(&self, s: &str) -> Result<(), String> {
        self.base.execute_batch(s)
    }
    fn last_insert_rowid(&self) -> i64 {
        self.base.last_insert_rowid()
    }
    fn write_tx(
        &self,
        f: &mut dyn FnMut(&dyn DbTxHandle) -> Result<(), String>,
    ) -> Result<(), String> {
        self.base.write_tx(f)
    }
    fn query_one(&self, s: &str, p: &[&dyn ToSqlValue]) -> Result<Option<Vec<SqlValue>>, String> {
        let t = Instant::now();
        let r = self.base.query_one(s, p);
        self.retenir(s, p, t);
        r
    }
    fn query_many(&self, s: &str, p: &[&dyn ToSqlValue]) -> Result<Vec<Vec<SqlValue>>, String> {
        let t = Instant::now();
        let r = self.base.query_many(s, p);
        self.retenir(s, p, t);
        r
    }
}

fn remplir(state: &AppState) {
    // Même distribution sur les deux moteurs ; bios de 2 Ko, DR, hi-res et canaux.
    let bio = "b".repeat(2000);
    let sql = format!("
    INSERT INTO artists(id,name) WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i<1000) SELECT i, substr('ABCDEFGHIJKLMNOPQRSTUVWXYZ',1+(i%26),1)||' Artiste '||i FROM n;
    INSERT INTO albums(id,title,artist_id,source,genre,year,bio)
      WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i<6000)
      SELECT i, substr('ABCDEFGHIJKLMNOPQRSTUVWXYZ',1+(i%26),1)||' Album '||i, 1+(i%1000), 'local', 'Jazz', 1950+(i%70), '{bio}' FROM n;
    INSERT INTO tracks(id,title,album_id,artist_id,disc_number,track_number,file_path,format,sample_rate,bit_depth,channels,source,duration_ms)
      WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i<72000)
      SELECT i,'Piste '||i,1+((i-1)/12),1+((1+((i-1)/12))%1000),1,1+((i-1)%12),'/synthetique/'||i||'.flac','flac',CASE WHEN i%3=0 THEN 192000 ELSE 44100 END,24,2,'local',240000 FROM n;
    INSERT INTO track_metadata(track_id,key,value) SELECT id,'dr_album',CAST(8+(id%12) AS TEXT) FROM tracks WHERE id%120=1;
     ");
    state
        .backend
        .write_tx(&mut |tx| {
            for statement in sql.split(';').map(str::trim).filter(|s| !s.is_empty()) {
                tx.execute(statement, &[])?;
            }
            Ok(())
        })
        .unwrap();
    state.backend.execute_batch("ANALYZE").unwrap();
    assert_eq!(
        state
            .backend
            .query_one("SELECT COUNT(*) FROM tracks", &[])
            .unwrap()
            .unwrap()[0]
            .as_i64(),
        Some(72000)
    );
}

#[test]
#[ignore = "banc HTTP et navigateur manuel, 6000 albums et 72000 pistes par moteur"]
fn servir_banc_albums_http_7774() {
    let preuves = std::path::PathBuf::from(
        std::env::var("BANC_7774_PREUVES").expect("dossier de preuves explicitement fourni"),
    );
    std::fs::create_dir_all(&preuves).unwrap();
    let dossier = tempfile::tempdir().unwrap();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let froid = std::env::var_os("BANC_7774_FROID").is_some();
        let mut sondes = Vec::new();
        let mut adresses=serde_json::Map::new();
        let mut taches=Vec::new();
        for moteur in ["sqlite","postgres"] {
            let mut config=TuneConfig::default();
            if moteur=="postgres" { config.database_url=Some(std::env::var("BANC_7774_PG_URL").expect("cluster PG synthétique dédié")); }
            let fichier=dossier.path().join(format!("{moteur}.db"));
            let mut state=AppState::new(fichier.to_str().unwrap(),0,config.clone()).unwrap();
            let t=Instant::now();
            remplir(&state);
            eprintln!("7774 {moteur} préparation {} s",t.elapsed().as_secs_f64());
            if froid {
                // État applicatif neuf : aucune route appelée après la création.
                // Le cache disque du système n’est pas purgé sur Shrek partagé.
                drop(state);
                state = AppState::new(fichier.to_str().unwrap(), 0, config).unwrap();
            }
            let mesure=Arc::new(MesureSql{base:state.backend.clone(),actif:AtomicBool::new(true),lectures:Mutex::new(Vec::new())});
            state.backend=mesure.clone();
            let app=tune_server::routes::router(state);
            let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let adresse=listener.local_addr().unwrap();
            taches.push(tokio::spawn(async move {axum::serve(listener,app).await.unwrap()}));
            adresses.insert(moteur.into(),serde_json::json!(format!("http://{adresse}")));
            if froid {
                sondes.push((moteur, mesure));
                continue;
            }
            let client=reqwest::Client::new();
            let mut mesures=Vec::new();
            for (limit,offset) in [(100,0),(1,3000),(100,3000),(2000,0),(2000,2000),(2000,4000)] {
                let url=format!("http://{adresse}/api/v1/library/albums?limit={limit}&offset={offset}&sort=title&order=asc");
                let t=Instant::now();
                let reponse=client.get(&url).send().await.unwrap();
                assert!(reponse.status().is_success(),"{url}: {}",reponse.status());
                let octets=reponse.bytes().await.unwrap();
                let ms=t.elapsed().as_secs_f64()*1000.;
                let json:serde_json::Value=serde_json::from_slice(&octets).unwrap();
                assert_eq!(json["items"].as_array().unwrap().len(),limit);
                assert_eq!(json["total"],6000);
                if std::env::var_os("BANC_7774_VERIFIER_REPERES").is_some() {
                    let reperes = json["reperes_alphabetiques"].as_object().expect("les repères A–Z sont livrés avec la page");
                    assert_eq!(reperes.len(), 26);
                    assert_eq!(reperes["A"], 0);
                    // 230 titres A (multiples de 26), 231 par lettre B..M.
                    assert_eq!(reperes["M"], 230 + 11 * 231);
                    assert_eq!(reperes["Z"], 5770);
                }
                mesures.push(serde_json::json!({"url":url,"ms":ms,"octets":octets.len()}));
                eprintln!("7774 {moteur} HTTP limit={limit} offset={offset}: {ms:.2} ms");
            }
            mesure.actif.store(false,Ordering::Relaxed);
            std::fs::write(preuves.join(format!("{moteur}-http.json")),serde_json::to_vec_pretty(&mesures).unwrap()).unwrap();
            let lectures=std::mem::take(&mut *mesure.lectures.lock().unwrap());
            let mut plans=Vec::new();
            for (sql,params,ms) in lectures {
                let refs:Vec<&dyn ToSqlValue>=params.iter().map(|p|p as &dyn ToSqlValue).collect();
                let prefixe=if moteur=="sqlite" {"EXPLAIN QUERY PLAN "} else {"EXPLAIN (ANALYZE, BUFFERS, FORMAT TEXT) "};
                let plan=mesure.base.query_many(&format!("{prefixe}{sql}"),&refs);
                plans.push(serde_json::json!({"sql":sql,"params":format!("{params:?}"),"ms":ms,"plan":format!("{plan:?}")}));
            }
            std::fs::write(preuves.join(format!("{moteur}-plans.json")),serde_json::to_vec_pretty(&plans).unwrap()).unwrap();
        }
        std::fs::write(preuves.join("adresses.json"),serde_json::to_vec_pretty(&adresses).unwrap()).unwrap();
        eprintln!("7774 PRÊT navigateur : {}",preuves.display());
        let debut=Instant::now();
        while !preuves.join("termine").exists() && debut.elapsed()<Duration::from_secs(3600) { tokio::time::sleep(Duration::from_secs(1)).await; }
        for (moteur, mesure) in sondes {
            mesure.actif.store(false, Ordering::Relaxed);
            let lectures = std::mem::take(&mut *mesure.lectures.lock().unwrap());
            let mut plans = Vec::new();
            for (sql, params, ms) in lectures {
                let refs: Vec<&dyn ToSqlValue> = params.iter().map(|p| p as &dyn ToSqlValue).collect();
                let prefixe = if moteur == "sqlite" { "EXPLAIN QUERY PLAN " } else { "EXPLAIN (ANALYZE, BUFFERS, FORMAT TEXT) " };
                let plan = mesure.base.query_many(&format!("{prefixe}{sql}"), &refs);
                plans.push(serde_json::json!({"sql":sql,"params":format!("{params:?}"),"ms":ms,"plan":format!("{plan:?}")}));
            }
            std::fs::write(preuves.join(format!("{moteur}-plans-froid.json")), serde_json::to_vec_pretty(&plans).unwrap()).unwrap();
        }
        for t in taches { t.abort(); }
    });
}

#[cfg(test)]
mod tests_reperes_alphabetiques_7774 {
    use axum::{
        body::{Body, to_bytes},
        http::Request,
    };
    use serde_json::{Value, json};
    use tower::ServiceExt;
    use tune_core::db::backend::ToSqlValue;

    async fn page(app: &axum::Router, requete: &str) -> Value {
        let reponse = app
            .clone()
            .oneshot(
                Request::get(format!("/api/v1/library/albums?{requete}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(reponse.status().is_success());
        serde_json::from_slice(&to_bytes(reponse.into_body(), usize::MAX).await.unwrap()).unwrap()
    }

    #[tokio::test]
    async fn les_reperes_portent_sur_toute_la_liste_triee_pas_sur_la_page_7774() {
        let etat = tune_server::state::AppState::new(":memory:", 0, Default::default()).unwrap();
        let db = &etat.backend;
        for (id, nom) in [(1_i64, "Émile"), (2, "Zaz"), (3, "Boris")] {
            db.execute(
                "INSERT INTO artists (id, name) VALUES (?, ?)",
                &[&id as &dyn ToSqlValue, &nom],
            )
            .unwrap();
        }
        // L'ordre d'insertion est volontairement différent de l'ordre du rail.
        for (id, titre, artiste, annee) in [
            (1_i64, "Zèbre", 1_i64, 2002_i64),
            (2, "«Été»", 2, 2001),
            (3, "10 morceaux", 3, 2000),
            (4, "Alpha", 1, 2000),
            (5, "Écho", 1, 2001),
        ] {
            db.execute("INSERT INTO albums (id, title, artist_id, year, source, track_count) VALUES (?, ?, ?, ?, 'local', 1)",
                &[&id as &dyn ToSqlValue, &titre, &artiste, &annee]).unwrap();
            let chemin = format!("/synthetique-7774/{id}.flac");
            db.execute("INSERT INTO tracks (title, album_id, artist_id, file_path, duration_ms) VALUES ('piste', ?, ?, ?, 1000)",
                &[&id as &dyn ToSqlValue, &artiste, &chemin]).unwrap();
        }
        let app = tune_server::routes::router(etat);
        let j = page(&app, "sort=title&order=asc&limit=1&offset=2").await;
        assert_eq!(j["total"], 5);
        assert_eq!(j["items"][0]["title"], "Écho");
        assert_eq!(j["reperes_alphabetiques"], json!({"#":0,"A":1,"E":2,"Z":4}));
        let j = page(&app, "sort=artist&order=asc&limit=1&offset=3").await;
        assert_eq!(j["items"][0]["title"], "Zèbre");
        assert_eq!(j["reperes_alphabetiques"], json!({"B":0,"E":1,"Z":4}));
        let j = page(&app, "sort=title&order=desc&limit=1").await;
        assert_eq!(j["items"][0]["title"], "Zèbre");
        assert_eq!(j["reperes_alphabetiques"], json!({"Z":0,"E":1,"A":3,"#":4}));
        let j = page(&app, "sort=title&limit=1&offset=99").await;
        assert_eq!(j["items"], json!([]));
        assert_eq!(j["total"], 5);
        assert_eq!(j["reperes_alphabetiques"], json!({"#":0,"A":1,"E":2,"Z":4}));
        let j = page(&app, "sort=added&limit=1").await;
        assert!(j.get("reperes_alphabetiques").is_none());
    }
}
