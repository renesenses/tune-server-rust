//! Les snapshots du greffon « Playlists converter » DANS le bac à sable
//! (#4718, épique #4715).
//!
//! Les essais de la caisse `tune-playlists-converter` jouent le moteur en
//! natif. Ce fichier charge le **vrai `main.wasm`** avec le vrai `WasmPlugin`,
//! derrière les permissions de son manifeste, et le conduit par
//! `handle_route` — le chemin exact de `/api/v1/plugins/playlists-converter/…`.
//!
//! Trois propriétés y sont prouvées bout en bout :
//!
//! 1. **Un snapshot est écrit AVANT le premier titre versé** par un transfert
//!    — on le lit dans l'ordre des appels reçus par l'hôte ;
//! 2. **le retour en arrière ne supprime rien** : il rajoute ce qui manque,
//!    laisse en place ce qui est en trop et le LISTE, et n'écrit rien sans
//!    accord ;
//! 3. **le snapshot est daté par l'hôte** (`host_now`) — un greffon wasm n'a
//!    pas d'horloge à lui.
//!
//! Le fixture `tests/fixtures/plugins/playlists-converter/main.wasm` se
//! reconstruit comme indiqué dans `greffon_convertisseur_4717.rs`.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use tune_plugin_runtime_wasm::{HostContext, Limits, WasmPlugin};

/// L'heure que l'hôte de banc annonce.
const HEURE_DE_L_HOTE_MS: u64 = 1_790_200_000_000;

// ---------------------------------------------------------------------------
// L'hôte de banc : des playlists qui bougent, et le journal de ce qui arrive
// ---------------------------------------------------------------------------

fn piste(id: &str, titre: &str, artiste: &str, duree_ms: u64) -> Value {
    json!({ "source_id": id, "title": titre, "artist_name": artiste, "duration_ms": duree_ms, "isrc": "" })
}

struct HoteDeBanc {
    kv: Mutex<HashMap<String, Value>>,
    /// Le contenu des playlists de service, par identifiant.
    playlists: Mutex<HashMap<String, (String, Vec<Value>)>>,
    /// Tout ce que l'hôte a reçu, dans l'ordre : `kv:<clé>`, `creation:<id>`,
    /// `ajout:<id>:<ids>`.
    operations: Mutex<Vec<String>>,
    creations: Mutex<usize>,
}

impl HoteDeBanc {
    fn new() -> Arc<Self> {
        let mut playlists = HashMap::new();
        playlists.insert(
            "pl-1".to_string(),
            (
                "Nuit blanche".to_string(),
                vec![
                    piste("s-1", "Imagine", "John Lennon", 183_000),
                    piste("s-2", "Come Together", "The Beatles", 259_000),
                    piste("s-3", "Fragile", "Sting", 232_000),
                ],
            ),
        );
        Arc::new(Self {
            kv: Mutex::new(HashMap::new()),
            playlists: Mutex::new(playlists),
            operations: Mutex::new(Vec::new()),
            creations: Mutex::new(0),
        })
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
            .map(|(_, p)| {
                p.iter()
                    .map(|v| v["source_id"].as_str().unwrap().to_string())
                    .collect()
            })
            .unwrap_or_default()
    }
    /// L'utilisateur retouche sa playlist chez le service.
    fn remplacer(&self, id: &str, pistes: Vec<Value>) {
        let mut p = self.playlists.lock().unwrap();
        let nom = p.get(id).map(|(n, _)| n.clone()).unwrap_or_default();
        p.insert(id.to_string(), (nom, pistes));
    }
    fn noter(&self, o: String) {
        self.operations.lock().unwrap().push(o);
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
        let p = self.playlists.lock().unwrap();
        let liste: Vec<Value> = p
            .iter()
            .map(|(id, (nom, pistes))| json!({ "source_id": id, "name": nom, "track_count": pistes.len() }))
            .collect();
        Ok(json!({ "service": service, "count": liste.len(), "playlists": liste }))
    }
    fn streaming_playlist_tracks(&self, service: &str, playlist_id: &str) -> Result<Value, String> {
        let p = self.playlists.lock().unwrap();
        let (_, pistes) = p
            .get(playlist_id)
            .ok_or_else(|| format!("playlist introuvable : {playlist_id}"))?;
        Ok(
            json!({ "service": service, "playlist_id": playlist_id, "count": pistes.len(), "tracks": pistes }),
        )
    }
    fn streaming_playlist_create(
        &self,
        service: &str,
        name: &str,
        _description: Option<&str>,
    ) -> Result<Value, String> {
        let id = {
            let mut n = self.creations.lock().unwrap();
            *n += 1;
            format!("cible-{n}")
        };
        self.playlists
            .lock()
            .unwrap()
            .insert(id.clone(), (name.to_string(), Vec::new()));
        self.noter(format!("creation:{id}"));
        Ok(json!({ "service": service, "playlist_id": id, "name": name }))
    }
    fn streaming_playlist_add_tracks(
        &self,
        _service: &str,
        playlist_id: &str,
        track_ids: Vec<String>,
    ) -> Result<Value, String> {
        self.noter(format!("ajout:{playlist_id}:{}", track_ids.join(",")));
        let mut p = self.playlists.lock().unwrap();
        let entree = p
            .entry(playlist_id.to_string())
            .or_insert_with(|| (playlist_id.to_string(), Vec::new()));
        for id in &track_ids {
            entree.1.push(piste(id, id, "", 200_000));
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
        match title {
            "Imagine" => Ok(json!({
                "service": service,
                "matched": piste("q-1", "Imagine", "John Lennon", 184_000),
                "score": 0.95,
                "approximate": false,
            })),
            _ => Ok(json!({ "service": service, "matched": Value::Null })),
        }
    }
    fn library_search(&self, _query: &str, _limit: i64) -> Result<Value, String> {
        Err("library : permission non demandée par ce greffon".to_string())
    }
    fn library_match_track(
        &self,
        _title: &str,
        _artist: &str,
        _isrc: &str,
        _duration_ms: u64,
    ) -> Result<Value, String> {
        Err("library : permission non demandée par ce greffon".to_string())
    }

    fn kv_get(&self, _plugin_id: &str, key: &str) -> Result<Value, String> {
        Ok(match self.kv.lock().unwrap().get(key) {
            Some(v) => json!({ "key": key, "found": true, "value": v }),
            None => json!({ "key": key, "found": false, "value": Value::Null }),
        })
    }
    fn kv_set(&self, _plugin_id: &str, key: &str, value: Value) -> Result<Value, String> {
        self.noter(format!("kv:{key}"));
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
        HEURE_DE_L_HOTE_MS
    }
}

// ---------------------------------------------------------------------------
// Chargement
// ---------------------------------------------------------------------------

fn fixture(nom: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/plugins/playlists-converter")
        .join(nom)
}

fn charger(hote: Arc<HoteDeBanc>) -> WasmPlugin {
    let brut = std::fs::read_to_string(fixture("manifest.json")).expect("manifeste");
    let m: Value = serde_json::from_str(&brut).expect("manifeste JSON");
    let permissions: HashSet<String> = m["permissions"]
        .as_array()
        .expect("permissions")
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
    let brut = p.handle_route(&req.to_string()).expect("dispatch");
    serde_json::from_str(&brut).expect("réponse JSON")
}

// ---------------------------------------------------------------------------
// Les essais
// ---------------------------------------------------------------------------

/// 🔴 Le transfert écrit la copie datée de la playlist visée AVANT le premier
/// titre versé, et le lot en garde l'identifiant.
#[test]
fn le_transfert_prend_un_snapshot_avant_de_verser_4718() {
    let hote = HoteDeBanc::new();
    let mut p = charger(hote.clone());
    let apercu = route(
        &mut p,
        "POST",
        "/apercu",
        "",
        json!({ "source_service": "tidal", "cible_service": "qobuz", "playlists": ["pl-1"] }),
    );
    let lot_id = apercu["body"]["lot"]["lot_id"]
        .as_str()
        .unwrap()
        .to_string();
    let r = route(
        &mut p,
        "POST",
        "/transfert",
        "",
        json!({ "lot_id": lot_id, "accord": true }),
    );
    assert_eq!(r["status"], 200, "{r}");

    let ops = hote.operations();
    let snapshot = ops
        .iter()
        .position(|o| o.starts_with("kv:snap:"))
        .expect("un snapshot est écrit");
    let ajout = ops
        .iter()
        .position(|o| o.starts_with("ajout:"))
        .expect("un titre est versé");
    assert!(snapshot < ajout, "snapshot AVANT le versement — vu {ops:?}");

    let id = r["body"]["lot"]["playlists"][0]["snapshot_avant"]
        .as_str()
        .expect("le lot garde l'identifiant du snapshot")
        .to_string();
    let s = route(&mut p, "GET", "/snapshot", &format!("id={id}"), Value::Null);
    assert_eq!(s["status"], 200, "{s}");
    assert_eq!(s["body"]["snapshot"]["service"], "qobuz");
    assert_eq!(
        s["body"]["snapshot"]["motif"],
        format!("avant_transfert:{lot_id}")
    );
}

/// 🔴 Le retour en arrière, à travers le bac à sable : aperçu sans écriture,
/// refus sans accord, puis RAJOUT de la piste manquante — et la piste en trop
/// reste, listée pour que l'utilisateur la retire lui-même.
#[test]
fn le_retour_en_arriere_rajoute_sans_rien_supprimer_4718() {
    let hote = HoteDeBanc::new();
    let mut p = charger(hote.clone());

    let r = route(
        &mut p,
        "POST",
        "/snapshot",
        "",
        json!({ "service": "tidal", "playlist_id": "pl-1" }),
    );
    assert_eq!(r["status"], 200, "{r}");
    assert_eq!(r["body"]["snapshot"]["nom"], "Nuit blanche");
    let snapshot_id = r["body"]["snapshot"]["snapshot_id"]
        .as_str()
        .unwrap()
        .to_string();

    // L'utilisateur retire « Come Together » et ajoute « Roxanne ».
    hote.remplacer(
        "pl-1",
        vec![
            piste("s-1", "Imagine", "John Lennon", 183_000),
            piste("s-3", "Fragile", "Sting", 232_000),
            piste("s-4", "Roxanne", "The Police", 192_000),
        ],
    );

    let apercu = route(
        &mut p,
        "POST",
        "/snapshot/restauration/apercu",
        "",
        json!({ "snapshot_id": snapshot_id, "mode": "completer" }),
    );
    assert_eq!(apercu["status"], 200, "{apercu}");
    assert_eq!(apercu["body"]["plan"]["a_rajouter_ids"], json!(["s-2"]));
    assert_eq!(apercu["body"]["a_retirer_par_vous"][0]["titre"], "Roxanne");
    assert!(hote.ajouts().is_empty(), "l'aperçu n'écrit rien");
    let plan_id = apercu["body"]["plan"]["plan_id"]
        .as_str()
        .unwrap()
        .to_string();

    let refus = route(
        &mut p,
        "POST",
        "/snapshot/restauration",
        "",
        json!({ "plan_id": plan_id, "accord": false }),
    );
    assert_eq!(refus["status"], 409, "{refus}");
    assert!(hote.ajouts().is_empty(), "rien sans accord");

    let fait = route(
        &mut p,
        "POST",
        "/snapshot/restauration",
        "",
        json!({ "plan_id": plan_id, "accord": true }),
    );
    assert_eq!(fait["status"], 200, "{fait}");
    assert_eq!(fait["body"]["plan"]["etat"], "termine");
    assert_eq!(hote.ajouts(), vec!["ajout:pl-1:s-2".to_string()]);
    assert_eq!(
        hote.ids_de("pl-1"),
        vec!["s-1", "s-3", "s-4", "s-2"],
        "« Roxanne » est toujours là : rien n'est supprimé"
    );
    assert_eq!(fait["body"]["a_retirer_par_vous"][0]["id"], "s-4");
}

/// Le snapshot porte l'heure de L'HÔTE, lue par `host_now`.
#[test]
fn le_snapshot_est_date_par_l_hote_4718() {
    let hote = HoteDeBanc::new();
    let mut p = charger(hote);
    let r = route(
        &mut p,
        "POST",
        "/snapshot",
        "",
        json!({ "service": "tidal", "playlist_id": "pl-1" }),
    );
    assert_eq!(r["body"]["snapshot"]["pris_le_ms"], HEURE_DE_L_HOTE_MS);

    let liste = route(
        &mut p,
        "GET",
        "/snapshots",
        "service=tidal&playlist_id=pl-1",
        Value::Null,
    );
    assert_eq!(liste["body"]["count"], 1);
    assert_eq!(liste["body"]["retention_par_playlist"], 10);
}
