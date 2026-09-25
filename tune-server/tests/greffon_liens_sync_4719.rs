//! Les liens auto-sync du greffon « Playlists converter » DANS le bac à sable
//! (#4719, épique #4715).
//!
//! Le vrai `main.wasm`, chargé par le vrai `WasmPlugin` derrière les
//! permissions de son manifeste, conduit par `handle_route` (les routes
//! `/api/v1/plugins/playlists-converter/…`) et par `on_event` avec
//! l'enveloppe exacte que produit le minuteur de l'hôte
//! (`plugins_host::spawn_wasm_minuteur`).
//!
//! Ce qui y est prouvé bout en bout :
//!
//! 1. la **première** synchronisation exige un aperçu, puis un accord ;
//! 2. une synchronisation — du minuteur comme d'une demande — **n'écrit que
//!    des ajouts** : une piste disparue d'un côté est signalée au journal et
//!    reste de l'autre ;
//! 3. un **snapshot** précède l'écriture ;
//! 4. le **journal** dit quand, quoi, combien ;
//! 5. un lien **en pause** n'est pas réveillé ; un lien **supprimé** ne
//!    touche pas aux playlists.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use tune_plugin_runtime_wasm::{HostContext, Limits, WasmPlugin};

const MINUTE: u64 = 60_000;

fn piste(id: &str, titre: &str, artiste: &str, duree_ms: u64) -> Value {
    json!({ "source_id": id, "title": titre, "artist_name": artiste, "duration_ms": duree_ms, "isrc": "" })
}

fn imagine_a() -> Value {
    piste("s-1", "Imagine", "John Lennon", 183_000)
}
fn fragile_a() -> Value {
    piste("s-2", "Fragile", "Sting", 232_000)
}
fn roxanne_a() -> Value {
    piste("s-4", "Roxanne", "The Police", 192_000)
}

// ---------------------------------------------------------------------------
// L'hôte de banc : deux services, des playlists qui bougent, une horloge
// ---------------------------------------------------------------------------

struct HoteDeBanc {
    kv: Mutex<HashMap<String, Value>>,
    playlists: Mutex<HashMap<String, Vec<Value>>>,
    operations: Mutex<Vec<String>>,
    horloge: Mutex<u64>,
}

impl HoteDeBanc {
    fn new() -> Arc<Self> {
        let mut playlists = HashMap::new();
        playlists.insert("pl-a".to_string(), vec![imagine_a(), fragile_a()]);
        playlists.insert(
            "pl-b".to_string(),
            vec![piste("q-1", "Imagine", "John Lennon", 184_000)],
        );
        Arc::new(Self {
            kv: Mutex::new(HashMap::new()),
            playlists: Mutex::new(playlists),
            operations: Mutex::new(Vec::new()),
            horloge: Mutex::new(1_790_200_000_000),
        })
    }
    fn avancer(&self, ms: u64) {
        *self.horloge.lock().unwrap() += ms;
    }
    fn operations(&self) -> Vec<String> {
        self.operations.lock().unwrap().clone()
    }
    fn ajouts(&self) -> Vec<String> {
        self.operations()
            .into_iter()
            .filter(|o| o.starts_with("ajout:"))
            .collect()
    }
    fn ids_de(&self, id: &str) -> Vec<String> {
        self.playlists
            .lock()
            .unwrap()
            .get(id)
            .map(|p| {
                p.iter()
                    .map(|v| v["source_id"].as_str().unwrap().to_string())
                    .collect()
            })
            .unwrap_or_default()
    }
    fn remplacer(&self, id: &str, pistes: Vec<Value>) {
        self.playlists
            .lock()
            .unwrap()
            .insert(id.to_string(), pistes);
    }
    /// Ce que chaque service rend pour un titre.
    fn catalogue(service: &str, titre: &str) -> Option<Value> {
        match (service, titre) {
            ("qobuz", "Imagine") => Some(piste("q-1", "Imagine", "John Lennon", 184_000)),
            ("qobuz", "Fragile") => Some(piste("q-2", "Fragile", "Sting", 232_500)),
            ("qobuz", "Roxanne") => Some(piste("q-4", "Roxanne", "The Police", 192_000)),
            ("tidal", "Imagine") => Some(imagine_a()),
            ("tidal", "Fragile") => Some(fragile_a()),
            ("tidal", "Roxanne") => Some(roxanne_a()),
            _ => None,
        }
    }
}

impl HostContext for HoteDeBanc {
    fn log(&self, _level: &str, _msg: &str) {}
    fn queue_get(&self, _zone: i64) -> Result<Value, String> {
        Err("hors périmètre".into())
    }
    fn queue_add(&self, _zone: i64, _tracks: Value) -> Result<Value, String> {
        Err("hors périmètre".into())
    }
    fn now_playing(&self, _zone: i64) -> Result<Value, String> {
        Err("hors périmètre".into())
    }
    fn play(&self, _zone: i64, _req: Value) -> Result<Value, String> {
        Err("hors périmètre".into())
    }
    fn pause(&self, _zone: i64) -> Result<Value, String> {
        Err("hors périmètre".into())
    }
    fn emit(&self, _event: &str, _payload: Value) {}

    fn playlists_list(&self, _limit: i64, _offset: i64) -> Result<Value, String> {
        Ok(json!({ "count": 0, "playlists": [] }))
    }
    fn playlist_tracks(&self, _playlist_id: i64) -> Result<Value, String> {
        Err("hors périmètre".into())
    }
    fn playlist_create(&self, _name: &str, _description: Option<&str>) -> Result<Value, String> {
        Err("hors périmètre".into())
    }
    fn playlist_add_tracks(&self, _id: i64, _ids: Vec<i64>) -> Result<Value, String> {
        Err("hors périmètre".into())
    }
    fn streaming_services(&self) -> Result<Value, String> {
        Ok(json!({ "count": 0, "services": [] }))
    }
    fn streaming_playlists(&self, service: &str) -> Result<Value, String> {
        let id = if service == "tidal" { "pl-a" } else { "pl-b" };
        Ok(
            json!({ "service": service, "count": 1, "playlists": [{ "source_id": id, "name": format!("Route 66 ({service})") }] }),
        )
    }
    fn streaming_playlist_tracks(&self, service: &str, playlist_id: &str) -> Result<Value, String> {
        let p = self.playlists.lock().unwrap();
        let pistes = p
            .get(playlist_id)
            .ok_or_else(|| format!("playlist introuvable : {playlist_id}"))?;
        Ok(
            json!({ "service": service, "playlist_id": playlist_id, "count": pistes.len(), "tracks": pistes }),
        )
    }
    fn streaming_playlist_create(
        &self,
        _service: &str,
        _name: &str,
        _description: Option<&str>,
    ) -> Result<Value, String> {
        self.operations.lock().unwrap().push("creation".to_string());
        Err("un lien ne crée pas de playlist".into())
    }
    fn streaming_playlist_add_tracks(
        &self,
        service: &str,
        playlist_id: &str,
        track_ids: Vec<String>,
    ) -> Result<Value, String> {
        self.operations
            .lock()
            .unwrap()
            .push(format!("ajout:{playlist_id}:{}", track_ids.join(",")));
        let mut p = self.playlists.lock().unwrap();
        let liste = p.entry(playlist_id.to_string()).or_default();
        for id in &track_ids {
            let titre = match id.as_str() {
                "q-2" | "s-2" => "Fragile",
                "q-4" | "s-4" => "Roxanne",
                _ => "Imagine",
            };
            liste.push(Self::catalogue(service, titre).unwrap_or_else(|| piste(id, id, "", 0)));
        }
        Ok(json!({ "ok": true, "added": track_ids.len(), "demandees": track_ids.len() }))
    }
    fn streaming_match_track(
        &self,
        service: &str,
        title: &str,
        _artist: &str,
        _isrc: &str,
        _duration_ms: u64,
    ) -> Result<Value, String> {
        Ok(match Self::catalogue(service, title) {
            Some(p) => {
                json!({ "service": service, "matched": p, "score": 0.95, "approximate": false })
            }
            None => json!({ "service": service, "matched": Value::Null }),
        })
    }
    fn library_search(&self, _query: &str, _limit: i64) -> Result<Value, String> {
        Ok(json!({ "count": 0, "tracks": [] }))
    }
    fn library_match_track(
        &self,
        _title: &str,
        _artist: &str,
        _isrc: &str,
        _duration_ms: u64,
    ) -> Result<Value, String> {
        Ok(json!({ "matched": Value::Null, "count": 0, "candidates": [] }))
    }
    fn kv_get(&self, _plugin_id: &str, key: &str) -> Result<Value, String> {
        Ok(match self.kv.lock().unwrap().get(key) {
            Some(v) => json!({ "key": key, "found": true, "value": v }),
            None => json!({ "key": key, "found": false, "value": Value::Null }),
        })
    }
    fn kv_set(&self, _plugin_id: &str, key: &str, value: Value) -> Result<Value, String> {
        self.operations.lock().unwrap().push(format!("kv:{key}"));
        self.kv.lock().unwrap().insert(key.to_string(), value);
        Ok(json!({ "ok": true, "key": key }))
    }
    fn kv_list(&self, _plugin_id: &str, prefix: &str) -> Result<Value, String> {
        let mut cles: Vec<String> = self
            .kv
            .lock()
            .unwrap()
            .keys()
            .filter(|k| k.starts_with(prefix))
            .cloned()
            .collect();
        cles.sort();
        Ok(json!({ "count": cles.len(), "keys": cles }))
    }
    fn now_ms(&self) -> u64 {
        *self.horloge.lock().unwrap()
    }
}

// ---------------------------------------------------------------------------
// Chargement et conduite
// ---------------------------------------------------------------------------

fn fixture(nom: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/plugins/playlists-converter")
        .join(nom)
}

fn manifeste() -> Value {
    serde_json::from_str(&std::fs::read_to_string(fixture("manifest.json")).unwrap()).unwrap()
}

fn charger(hote: Arc<HoteDeBanc>) -> WasmPlugin {
    let permissions: HashSet<String> = manifeste()["permissions"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect();
    WasmPlugin::load_with_host(
        &fixture("main.wasm"),
        Limits::default(),
        hote,
        permissions,
        "playlists-converter",
    )
    .expect("le greffon se charge")
}

fn route(p: &mut WasmPlugin, methode: &str, chemin: &str, query: &str, corps: Value) -> Value {
    let req = json!({ "method": methode, "path": chemin, "query": query, "body": corps });
    serde_json::from_str(&p.handle_route(&req.to_string()).expect("dispatch")).expect("JSON")
}

/// L'enveloppe exacte du minuteur de l'hôte (`evenement_minuteur`).
fn minuteur(p: &mut WasmPlugin) {
    p.on_event(r#"{"name":"minuteur","payload":{"now_ms":0}}"#)
        .expect("plugin_on_event");
}

/// Créer le lien A (tidal) → B (qobuz), cadence 15 min, et le faire accepter.
fn lien_accepte(p: &mut WasmPlugin) -> String {
    let r = route(
        p,
        "POST",
        "/liens",
        "",
        json!({
            "a": { "service": "tidal", "playlist_id": "pl-a" },
            "b": { "service": "qobuz", "playlist_id": "pl-b" },
            "sens": "a_vers_b",
            "cadence_minutes": 15,
        }),
    );
    assert_eq!(r["status"], 200, "{r}");
    let id = r["body"]["lien"]["lien_id"].as_str().unwrap().to_string();
    let r = route(p, "POST", "/lien/apercu", "", json!({ "lien_id": id }));
    assert_eq!(r["status"], 200, "{r}");
    let r = route(
        p,
        "POST",
        "/lien/synchroniser",
        "",
        json!({ "lien_id": id, "accord": true }),
    );
    assert_eq!(r["status"], 200, "{r}");
    id
}

// ---------------------------------------------------------------------------
// Les essais
// ---------------------------------------------------------------------------

/// Le manifeste s'abonne au minuteur — et à RIEN d'autre du bus.
#[test]
fn le_manifeste_s_abonne_au_seul_minuteur_4719() {
    let m = manifeste();
    assert_eq!(m["event_subscriptions"], json!(["minuteur"]));
    assert_eq!(m["premium"], true);
}

/// 🔴 Première synchronisation : aperçu requis, puis accord ; l'aperçu
/// n'écrit rien ; le snapshot précède le premier ajout.
#[test]
fn la_premiere_synchro_exige_un_apercu_accepte_4719() {
    let hote = HoteDeBanc::new();
    let mut p = charger(hote.clone());
    let r = route(
        &mut p,
        "POST",
        "/liens",
        "",
        json!({
            "a": { "service": "tidal", "playlist_id": "pl-a" },
            "b": { "service": "qobuz", "playlist_id": "pl-b" },
            "cadence_minutes": 15,
        }),
    );
    let id = r["body"]["lien"]["lien_id"].as_str().unwrap().to_string();

    let r = route(
        &mut p,
        "POST",
        "/lien/synchroniser",
        "",
        json!({ "lien_id": id, "accord": true }),
    );
    assert_eq!(r["status"], 409, "sans aperçu : {r}");

    let r = route(&mut p, "POST", "/lien/apercu", "", json!({ "lien_id": id }));
    assert_eq!(r["body"]["plan"]["ajouts"][0]["cible_id"], "q-2");
    assert!(hote.ajouts().is_empty(), "l'aperçu n'écrit rien");

    hote.avancer(60 * MINUTE);
    minuteur(&mut p);
    assert!(
        hote.ajouts().is_empty(),
        "le minuteur ne touche pas un lien non accepté"
    );

    let r = route(
        &mut p,
        "POST",
        "/lien/synchroniser",
        "",
        json!({ "lien_id": id, "accord": false }),
    );
    assert_eq!(r["status"], 409, "sans accord : {r}");

    let r = route(
        &mut p,
        "POST",
        "/lien/synchroniser",
        "",
        json!({ "lien_id": id, "accord": true }),
    );
    assert_eq!(r["status"], 200, "{r}");
    assert_eq!(hote.ajouts(), vec!["ajout:pl-b:q-2".to_string()]);
    let ops = hote.operations();
    let snapshot = ops.iter().position(|o| o.starts_with("kv:snap:")).unwrap();
    let ajout = ops.iter().position(|o| o.starts_with("ajout:")).unwrap();
    assert!(snapshot < ajout, "snapshot AVANT l'écriture — {ops:?}");
}

/// 🔴 Le minuteur n'écrit que des AJOUTS ; la piste retirée de A reste dans
/// B, et le journal la signale.
#[test]
fn le_minuteur_n_ecrit_que_des_ajouts_4719() {
    let hote = HoteDeBanc::new();
    let mut p = charger(hote.clone());
    let id = lien_accepte(&mut p);

    // L'utilisateur retire « Fragile » de A et y ajoute « Roxanne ».
    hote.remplacer("pl-a", vec![imagine_a(), roxanne_a()]);
    hote.avancer(MINUTE);
    minuteur(&mut p);
    assert_eq!(hote.ajouts().len(), 1, "pas encore dû");

    hote.avancer(15 * MINUTE);
    minuteur(&mut p);
    assert_eq!(
        hote.ajouts(),
        vec!["ajout:pl-b:q-2".to_string(), "ajout:pl-b:q-4".to_string()]
    );
    assert_eq!(
        hote.ids_de("pl-b"),
        vec!["q-1", "q-2", "q-4"],
        "« Fragile » reste dans B : jamais de suppression"
    );
    assert!(!hote.operations().contains(&"creation".to_string()));

    let j = route(
        &mut p,
        "GET",
        "/lien/journal",
        &format!("id={id}"),
        Value::Null,
    );
    assert_eq!(j["status"], 200, "{j}");
    assert_eq!(j["body"]["count"], 2);
    let e = &j["body"]["entrees"][0];
    assert_eq!(e["declencheur"], "minuteur");
    assert_eq!(e["ajoutees"], 1);
    assert_eq!(e["ajouts"][0]["titre"], "Roxanne");
    assert_eq!(e["disparues_signalees"][0]["disparue_de"], "a");
    assert_eq!(e["disparues_signalees"][0]["id_restant"], "q-2");
    assert_eq!(e["snapshots"].as_array().unwrap().len(), 2);
    assert!(e["quand_ms"].as_u64().unwrap() > 0);
}

/// Un lien en pause n'est pas réveillé ; repris, il l'est.
#[test]
fn un_lien_en_pause_n_est_pas_reveille_4719() {
    let hote = HoteDeBanc::new();
    let mut p = charger(hote.clone());
    let id = lien_accepte(&mut p);
    let r = route(
        &mut p,
        "POST",
        "/lien/pause",
        "",
        json!({ "lien_id": id, "pause": true }),
    );
    assert_eq!(r["body"]["lien"]["etat"], "en_pause");

    hote.remplacer("pl-a", vec![imagine_a(), fragile_a(), roxanne_a()]);
    hote.avancer(60 * MINUTE);
    minuteur(&mut p);
    assert_eq!(hote.ajouts().len(), 1, "rien pendant la pause");

    route(
        &mut p,
        "POST",
        "/lien/pause",
        "",
        json!({ "lien_id": id, "pause": false }),
    );
    hote.avancer(16 * MINUTE);
    minuteur(&mut p);
    assert_eq!(hote.ajouts().len(), 2, "repris, il est réveillé");
}

/// Supprimer le LIEN ne touche à aucune des deux playlists.
#[test]
fn supprimer_un_lien_ne_touche_pas_aux_playlists_4719() {
    let hote = HoteDeBanc::new();
    let mut p = charger(hote.clone());
    let id = lien_accepte(&mut p);
    let a = hote.ids_de("pl-a");
    let b = hote.ids_de("pl-b");
    let avant = hote.ajouts().len();

    let r = route(
        &mut p,
        "POST",
        "/lien/supprimer",
        "",
        json!({ "lien_id": id }),
    );
    assert_eq!(r["status"], 200, "{r}");
    assert_eq!(r["body"]["playlists_touchees"], false);

    hote.remplacer("pl-a", vec![imagine_a(), fragile_a(), roxanne_a()]);
    hote.avancer(60 * MINUTE);
    minuteur(&mut p);
    assert_eq!(hote.ajouts().len(), avant);
    assert_eq!(hote.ids_de("pl-b"), b);
    assert_ne!(hote.ids_de("pl-a"), a, "seul l'utilisateur a touché A");
    let l = route(&mut p, "GET", "/liens", "", Value::Null);
    assert_eq!(l["body"]["count"], 0);
}
