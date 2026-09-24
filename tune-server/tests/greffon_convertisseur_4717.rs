//! Le greffon « Playlists converter » DANS le bac à sable (#4717, épique #4715).
//!
//! Les essais de la caisse `tune-playlists-converter` jouent son moteur en
//! natif. Ils ne disent rien de l'ABI : allocation dans la mémoire linéaire,
//! empaquetage `(ptr << 32) | len`, traduction des `extern "C"` du module
//! `"tune"`. Ce fichier-là charge le **vrai `main.wasm`** avec le vrai
//! `WasmPlugin`, derrière les vraies permissions, et le conduit par
//! `handle_route` — le chemin exact qu'emprunte `/api/v1/plugins/{id}/…`.
//!
//! Trois propriétés y sont prouvées BOUT EN BOUT :
//!
//! 1. **`POST /apercu` n'écrit rien chez le service.** L'hôte de banc compte
//!    les créations et les versements ; après l'aperçu, les deux compteurs
//!    sont à zéro. Ce n'est pas une lecture du code : c'est ce que l'hôte a
//!    réellement reçu.
//! 2. **`POST /transfert` sans accord n'écrit rien non plus**, et rend 409.
//! 3. **Avec accord, la playlist est créée et les titres appariés versés** —
//!    et la règle des ±3 s a bien écarté le remaster, à travers l'ABI.
//!
//! Le fixture `tests/fixtures/plugins/playlists-converter/main.wasm` se
//! reconstruit par :
//!
//! ```sh
//! cd plugins/tune-playlists-converter
//! RUSTFLAGS="-C opt-level=z -C codegen-units=1 -C panic=abort -C strip=symbols" \
//!   cargo build --target wasm32-unknown-unknown --release --lib
//! cp <target>/wasm32-unknown-unknown/release/tune_playlists_converter.wasm \
//!    ../../tune-server/tests/fixtures/plugins/playlists-converter/main.wasm
//! ```

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use tune_plugin_runtime_wasm::{HostContext, Limits, WasmPlugin};

// ---------------------------------------------------------------------------
// L'hôte de banc
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Journal {
    creations: Vec<String>,
    ajouts: Vec<(String, Vec<String>)>,
}

struct HoteDeBanc {
    kv: Mutex<HashMap<String, Value>>,
    journal: Mutex<Journal>,
}

impl HoteDeBanc {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            kv: Mutex::new(HashMap::new()),
            journal: Mutex::new(Journal::default()),
        })
    }
    fn creations(&self) -> Vec<String> {
        self.journal.lock().unwrap().creations.clone()
    }
    fn ajouts(&self) -> Vec<(String, Vec<String>)> {
        self.journal.lock().unwrap().ajouts.clone()
    }
}

/// La playlist source du banc : trois titres, dont un remaster de 9 s de trop
/// et un absent de la cible.
fn pistes_source() -> Value {
    json!([
        {"source_id": "s-1", "title": "Imagine", "artist_name": "John Lennon", "duration_ms": 183_000, "isrc": ""},
        {"source_id": "s-2", "title": "Come Together", "artist_name": "The Beatles", "duration_ms": 259_000, "isrc": ""},
        {"source_id": "s-3", "title": "Obscure", "artist_name": "Inconnu", "duration_ms": 200_000, "isrc": ""},
    ])
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
    fn playlist_tracks(&self, playlist_id: i64) -> Result<Value, String> {
        Ok(json!({
            "playlist_id": playlist_id,
            "name": "Nuit blanche",
            "count": 3,
            "tracks": pistes_source(),
        }))
    }
    fn playlist_create(&self, _name: &str, _description: Option<&str>) -> Result<Value, String> {
        Err("hors périmètre".into())
    }
    fn playlist_add_tracks(&self, _id: i64, _ids: Vec<i64>) -> Result<Value, String> {
        Err("hors périmètre".into())
    }

    fn streaming_services(&self) -> Result<Value, String> {
        Ok(json!({ "count": 2, "services": [
            {"name": "tidal", "authenticated": true, "supports_write": true},
            {"name": "qobuz", "authenticated": true, "supports_write": true},
        ]}))
    }
    fn streaming_playlists(&self, service: &str) -> Result<Value, String> {
        Ok(json!({
            "service": service,
            "count": 1,
            "playlists": [{ "source_id": "pl-1", "name": "Nuit blanche", "track_count": 3 }],
        }))
    }
    fn streaming_playlist_tracks(&self, service: &str, playlist_id: &str) -> Result<Value, String> {
        Ok(json!({
            "service": service,
            "playlist_id": playlist_id,
            "count": 3,
            "tracks": pistes_source(),
        }))
    }
    fn streaming_playlist_create(
        &self,
        service: &str,
        name: &str,
        _description: Option<&str>,
    ) -> Result<Value, String> {
        self.journal
            .lock()
            .unwrap()
            .creations
            .push(name.to_string());
        Ok(json!({ "service": service, "playlist_id": "cible-1", "name": name }))
    }
    fn streaming_playlist_add_tracks(
        &self,
        _service: &str,
        playlist_id: &str,
        track_ids: Vec<String>,
    ) -> Result<Value, String> {
        self.journal
            .lock()
            .unwrap()
            .ajouts
            .push((playlist_id.to_string(), track_ids.clone()));
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
        // Les verdicts du banc, calqués sur ce que rend l'hôte réel.
        match title {
            "Imagine" => Ok(json!({
                "service": service,
                "matched": {"source_id": "q-1", "title": "Imagine", "artist_name": "John Lennon", "duration_ms": 184_000},
                "score": 0.95,
                "approximate": false,
            })),
            // Titre et artiste concordent (le matcher retire « (Remastered) »),
            // mais 268 s contre 259 : neuf secondes de trop.
            "Come Together" => Ok(json!({
                "service": service,
                "matched": {"source_id": "q-2", "title": "Come Together (Remastered 2009)", "artist_name": "The Beatles", "duration_ms": 268_000},
                "score": 0.95,
                "approximate": false,
            })),
            _ => Ok(json!({ "service": service, "matched": Value::Null })),
        }
    }

    // La permission `library` (#4716, appariement LOCAL) est arrivée dans le
    // lot après ce greffon. Sa fiche ne la demande pas : le bac à sable refuse
    // l'appel avant d'atteindre l'hôte, et ce banc ne doit donc jamais la voir.
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
}

// ---------------------------------------------------------------------------
// Chargement
// ---------------------------------------------------------------------------

fn chemin_du_fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/plugins/playlists-converter/main.wasm")
}

/// Les permissions du manifeste livré, lues DANS le manifeste — pas recopiées
/// à la main : une permission ajoutée au fichier sans être voulue ferait ainsi
/// bouger l'essai.
fn permissions_du_manifeste() -> HashSet<String> {
    let brut = std::fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/plugins/playlists-converter/manifest.json"),
    )
    .expect("manifeste lisible");
    let m: Value = serde_json::from_str(&brut).expect("manifeste JSON");
    m["permissions"]
        .as_array()
        .expect("permissions")
        .iter()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect()
}

fn charger(hote: Arc<HoteDeBanc>) -> WasmPlugin {
    WasmPlugin::load_with_host(
        &chemin_du_fixture(),
        Limits::default(),
        hote,
        permissions_du_manifeste(),
        "playlists-converter",
    )
    .expect("le greffon se charge")
}

fn route(p: &mut WasmPlugin, methode: &str, chemin: &str, query: &str, corps: Value) -> Value {
    let req = json!({ "method": methode, "path": chemin, "query": query, "body": corps });
    let brut = p.handle_route(&req.to_string()).expect("dispatch");
    serde_json::from_str(&brut).expect("réponse JSON")
}

fn demande() -> Value {
    json!({ "source_service": "tidal", "cible_service": "qobuz", "playlists": ["pl-1"] })
}

// ---------------------------------------------------------------------------
// Les essais
// ---------------------------------------------------------------------------

/// Le manifeste dit ce que le ticket demande : greffon PREMIUM, et aucune
/// permission de plus que les trois de la tranche 1.
#[test]
fn le_manifeste_est_premium_et_ne_demande_que_trois_permissions() {
    let brut = std::fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/plugins/playlists-converter/manifest.json"),
    )
    .unwrap();
    let m: Value = serde_json::from_str(&brut).unwrap();
    assert_eq!(m["id"], "playlists-converter");
    assert_eq!(m["premium"], true, "décision de Bertrand du 22/09/2026");
    assert_eq!(m["entry_point"], "main.wasm");
    let mut perms = permissions_du_manifeste().into_iter().collect::<Vec<_>>();
    perms.sort();
    assert_eq!(perms, vec!["kv", "playlists", "streaming"]);
    // Aucun abonnement au bus : ce greffon ne réagit à rien, il répond.
    assert!(m.get("event_subscriptions").is_none());
}

/// L'ABI répond : le module se charge, donc `abi_version()` vaut celle de
/// l'hôte, et `alloc`/`dealloc`/`plugin_dispatch` sont bien exportés.
#[test]
fn le_greffon_se_charge_et_repond_a_une_route_inconnue() {
    let hote = HoteDeBanc::new();
    let mut p = charger(hote);
    let r = route(&mut p, "GET", "/inexistante", "", Value::Null);
    assert_eq!(r["status"], 404);
}

/// 🔴 L'aperçu, à travers le bac à sable, **n'écrit rien chez le service**.
#[test]
fn l_apercu_traverse_le_bac_a_sable_sans_ecrire_chez_le_service() {
    let hote = HoteDeBanc::new();
    let mut p = charger(hote.clone());

    let r = route(&mut p, "POST", "/apercu", "", demande());
    assert_eq!(r["status"], 200, "{r}");
    assert_eq!(r["body"]["resume"]["titres"], 3);
    assert_eq!(r["body"]["resume"]["appariees"], 1);
    assert_eq!(r["body"]["resume"]["introuvables"], 2);
    assert_eq!(r["body"]["resume"]["versees"], 0);

    assert!(hote.creations().is_empty(), "aucune playlist créée");
    assert!(hote.ajouts().is_empty(), "aucun titre versé");
}

/// La règle des ±3 s franchit l'ABI : le remaster ressort en introuvable avec
/// son écart, et la raison est lisible côté client.
#[test]
fn la_regle_des_trois_secondes_franchit_l_abi() {
    let hote = HoteDeBanc::new();
    let mut p = charger(hote);
    let r = route(&mut p, "POST", "/apercu", "", demande());

    let introuvables = r["body"]["lot"]["playlists"][0]["introuvables"]
        .as_array()
        .expect("des introuvables")
        .clone();
    let remaster = introuvables
        .iter()
        .find(|i| i["source_titre"] == "Come Together")
        .expect("le remaster est déclaré introuvable");
    assert_eq!(remaster["raison"]["code"], "duree_hors_tolerance");
    assert_eq!(remaster["raison"]["ecart_ms"], 9_000);

    let absent = introuvables
        .iter()
        .find(|i| i["source_titre"] == "Obscure")
        .expect("le titre absent est déclaré introuvable");
    assert_eq!(absent["raison"]["code"], "aucun_resultat");
}

/// 🔴 Sans accord, rien n'est écrit — et le code HTTP le dit.
#[test]
fn un_transfert_sans_accord_ne_cree_rien_a_travers_le_bac_a_sable() {
    let hote = HoteDeBanc::new();
    let mut p = charger(hote.clone());
    let apercu = route(&mut p, "POST", "/apercu", "", demande());
    let lot_id = apercu["body"]["lot"]["lot_id"]
        .as_str()
        .unwrap()
        .to_string();

    let r = route(
        &mut p,
        "POST",
        "/transfert",
        "",
        json!({ "lot_id": lot_id, "accord": false }),
    );
    assert_eq!(r["status"], 409, "{r}");
    assert!(hote.creations().is_empty());
    assert!(hote.ajouts().is_empty());
}

/// Avec l'accord : une playlist créée, le seul titre apparié versé.
#[test]
fn avec_accord_le_transfert_cree_et_verse_a_travers_le_bac_a_sable() {
    let hote = HoteDeBanc::new();
    let mut p = charger(hote.clone());
    let apercu = route(&mut p, "POST", "/apercu", "", demande());
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
    assert_eq!(r["body"]["resume"]["versees"], 1);
    assert_eq!(r["body"]["resume"]["etat"], "termine");

    assert_eq!(hote.creations(), vec!["Nuit blanche".to_string()]);
    assert_eq!(
        hote.ajouts(),
        vec![("cible-1".to_string(), vec!["q-1".to_string()])]
    );
}

/// L'état du lot survit dans le stockage cloisonné : `/lots` et `/lot?id=` le
/// relisent, et une reprise d'un lot terminé ne réécrit rien.
#[test]
fn l_etat_du_lot_se_relit_et_la_reprise_n_ecrit_rien_de_plus() {
    let hote = HoteDeBanc::new();
    let mut p = charger(hote.clone());
    let apercu = route(&mut p, "POST", "/apercu", "", demande());
    let lot_id = apercu["body"]["lot"]["lot_id"]
        .as_str()
        .unwrap()
        .to_string();
    route(
        &mut p,
        "POST",
        "/transfert",
        "",
        json!({ "lot_id": lot_id, "accord": true }),
    );

    let lots = route(&mut p, "GET", "/lots", "", Value::Null);
    assert_eq!(lots["status"], 200);
    assert_eq!(lots["body"]["count"], 1);

    let detail = route(&mut p, "GET", "/lot", &format!("id={lot_id}"), Value::Null);
    assert_eq!(detail["status"], 200);
    assert_eq!(detail["body"]["lot"]["lot_id"], lot_id.as_str());

    let avant = hote.ajouts().len();
    let reprise = route(&mut p, "POST", "/reprise", "", json!({ "lot_id": lot_id }));
    assert_eq!(reprise["status"], 200, "{reprise}");
    assert_eq!(hote.ajouts().len(), avant, "rien n'est reversé");
    assert_eq!(hote.creations().len(), 1, "rien n'est recréé");
}

/// 🔴 La garde qui compte le plus : **le greffon n'importe aucune capacité de
/// suppression**. On n'inspecte pas le texte du fichier — on charge le module
/// avec un jeu de permissions VIDE et on vérifie qu'il se charge quand même.
///
/// Pourquoi c'est la preuve : le `Linker` de l'hôte n'offre que les fonctions
/// qu'il a posées. Un module qui importerait une fonction inexistante — un
/// `host_playlist_delete`, par exemple — échouerait à l'instanciation, quelles
/// que soient ses permissions. Le chargement réussi dit donc que **tout ce que
/// ce `main.wasm` importe existe dans la surface de la tranche 1**, laquelle
/// ne porte aucune suppression (garde `aucune_capacite_hote_ne_supprime_4716`).
#[test]
fn le_greffon_n_importe_que_des_capacites_qui_existent() {
    let hote = HoteDeBanc::new();
    let mut p = WasmPlugin::load_with_host(
        &chemin_du_fixture(),
        Limits::default(),
        hote.clone(),
        HashSet::new(),
        "playlists-converter",
    )
    .expect("le module s'instancie même sans aucune permission");

    // Et sans permission, il ne fait rien : l'hôte refuse chaque appel.
    let r = route(&mut p, "POST", "/apercu", "", demande());
    assert_ne!(r["status"], 200, "{r}");
    assert!(hote.creations().is_empty());
}
