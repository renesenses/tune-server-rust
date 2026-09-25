//! L'ABI wasm (RFC §3.2/§3.3) — compilé pour `wasm32` uniquement.
//!
//! Tout ce qui est ici est de la plomberie : allouer, lire et écrire dans la
//! mémoire linéaire, et traduire les `extern "C"` du module `"tune"` en
//! méthodes de [`Hote`]. Aucune décision produit ne se prend dans ce fichier —
//! elles sont toutes dans `moteur.rs`, qui se joue en natif contre un double.
//!
//! Convention de retour, imposée par l'hôte : une valeur rendue est un
//! `u64 = (ptr << 32) | len` désignant du JSON UTF-8 dans la mémoire du
//! greffon. L'hôte lit, puis appelle `dealloc`.

use serde_json::Value;

use crate::hote::Hote;

/// La version d'ABI que l'hôte exige (`HOST_ABI_VERSION`). Un écart et le
/// greffon est refusé au chargement, ce qui est le comportement voulu.
const ABI: u32 = 1;

// 🔴 `wasm_import_module = "tune"` n'est PAS décoratif.
//
// Sans cet attribut, `extern "C"` place les imports dans le module `env` —
// le défaut de Rust — et l'instanciation échoue net :
// `unknown import: env::host_playlist_tracks has not been defined`. L'hôte
// n'installe ses fonctions que sous `"tune"` (RFC §3.4). Mesuré : les sept
// essais de `greffon_convertisseur_4717` ont rougi ainsi avant l'ajout.
//
// C'est aussi pourquoi cet essai existe : aucun essai natif ne peut voir ce
// défaut, puisque `abi.rs` n'est même pas compilé hors `wasm32`.
#[link(wasm_import_module = "tune")]
unsafe extern "C" {
    /// `{"level": "...", "msg": "..."}` — comme toutes les autres, en JSON.
    /// Elle est la seule à ne rien rendre.
    fn host_log(ptr: u32, len: u32);
    fn host_now(ptr: u32, len: u32) -> u64;
    fn host_playlist_tracks(ptr: u32, len: u32) -> u64;
    fn host_playlist_create(ptr: u32, len: u32) -> u64;
    fn host_playlist_add_tracks(ptr: u32, len: u32) -> u64;
    fn host_streaming_playlists(ptr: u32, len: u32) -> u64;
    fn host_streaming_playlist_tracks(ptr: u32, len: u32) -> u64;
    fn host_streaming_playlist_create(ptr: u32, len: u32) -> u64;
    fn host_streaming_playlist_add_tracks(ptr: u32, len: u32) -> u64;
    fn host_streaming_match_track(ptr: u32, len: u32) -> u64;
    fn host_library_match_track(ptr: u32, len: u32) -> u64;
    fn host_kv_get(ptr: u32, len: u32) -> u64;
    fn host_kv_set(ptr: u32, len: u32) -> u64;
    fn host_kv_list(ptr: u32, len: u32) -> u64;
}

// ---------------------------------------------------------------------------
// Mémoire
// ---------------------------------------------------------------------------

/// Réserver `len` octets pour l'hôte.
///
/// La capacité est mise à `len` exactement (`with_capacity` peut arrondir) :
/// `dealloc` reconstruit le `Vec` avec la même longueur, et un écart entre
/// capacité réservée et capacité rendue serait indéfini.
#[unsafe(no_mangle)]
pub extern "C" fn alloc(len: u32) -> u32 {
    let mut buf = Vec::<u8>::with_capacity(len as usize);
    let ptr = buf.as_mut_ptr();
    core::mem::forget(buf);
    ptr as u32
}

#[unsafe(no_mangle)]
pub extern "C" fn dealloc(ptr: u32, len: u32) {
    if ptr == 0 {
        return;
    }
    unsafe {
        drop(Vec::from_raw_parts(ptr as *mut u8, 0, len as usize));
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn abi_version() -> u32 {
    ABI
}

/// Le point d'entrée des routes : JSON en entrée, JSON en sortie.
#[unsafe(no_mangle)]
pub extern "C" fn plugin_dispatch(ptr: u32, len: u32) -> u64 {
    let entree = unsafe { lire(ptr, len) };
    let requete: Value = serde_json::from_slice(&entree).unwrap_or(Value::Null);
    let reponse = crate::dispatch::repondre(&HoteWasm, &requete);
    ecrire(&serde_json::to_vec(&reponse).unwrap_or_else(|_| b"{}".to_vec()))
}

/// Le point d'entrée des événements (RFC §3.6) — ici, le seul qui compte :
/// le `minuteur` que l'hôte envoie toutes les minutes au greffon abonné
/// (#4719). Rien n'est rendu : un événement est « tiré et oublié ».
#[unsafe(no_mangle)]
pub extern "C" fn plugin_on_event(ptr: u32, len: u32) {
    let entree = unsafe { lire(ptr, len) };
    let evenement: Value = serde_json::from_slice(&entree).unwrap_or(Value::Null);
    crate::dispatch::sur_evenement(&HoteWasm, &evenement);
}

unsafe fn lire(ptr: u32, len: u32) -> Vec<u8> {
    if ptr == 0 || len == 0 {
        return Vec::new();
    }
    unsafe { core::slice::from_raw_parts(ptr as *const u8, len as usize).to_vec() }
}

/// Poser un tampon en mémoire et rendre `(ptr << 32) | len`.
fn ecrire(octets: &[u8]) -> u64 {
    let mut buf = octets.to_vec();
    buf.shrink_to_fit();
    let ptr = buf.as_mut_ptr() as u64;
    let len = buf.len() as u64;
    core::mem::forget(buf);
    (ptr << 32) | len
}

/// Appeler une fonction hôte qui prend du JSON et rend du JSON.
fn appel(f: unsafe extern "C" fn(u32, u32) -> u64, requete: &Value) -> Result<Value, String> {
    let octets = serde_json::to_vec(requete).map_err(|e| e.to_string())?;
    let empaquete = unsafe { f(octets.as_ptr() as u32, octets.len() as u32) };
    let ptr = (empaquete >> 32) as u32;
    let len = (empaquete & 0xFFFF_FFFF) as u32;
    let brut = unsafe { lire(ptr, len) };
    dealloc(ptr, len);
    let valeur: Value =
        serde_json::from_slice(&brut).map_err(|e| format!("réponse hôte illisible : {e}"))?;
    // L'hôte signale une erreur LOGIQUE par `{"error": "…"}` plutôt que par un
    // trap, pour que le greffon puisse la présenter. `permission_denied` en
    // fait partie : une permission absente du manifeste se voit ici.
    if let Some(e) = valeur.get("error").and_then(Value::as_str) {
        return Err(e.to_string());
    }
    Ok(valeur)
}

// ---------------------------------------------------------------------------
// L'hôte réel
// ---------------------------------------------------------------------------

struct HoteWasm;

impl Hote for HoteWasm {
    fn journal(&self, niveau: &str, message: &str) {
        let Ok(octets) = serde_json::to_vec(&serde_json::json!({
            "level": niveau,
            "msg": message,
        })) else {
            return;
        };
        unsafe {
            host_log(octets.as_ptr() as u32, octets.len() as u32);
        }
    }

    fn maintenant_ms(&self) -> u64 {
        appel(host_now, &serde_json::json!({}))
            .ok()
            .and_then(|v| v.get("now_ms").and_then(Value::as_u64))
            .unwrap_or(0)
    }

    fn playlist_tracks(&self, playlist_id: i64) -> Result<Value, String> {
        appel(
            host_playlist_tracks,
            &serde_json::json!({ "playlist_id": playlist_id }),
        )
    }

    fn playlist_create(&self, name: &str, description: Option<&str>) -> Result<Value, String> {
        appel(
            host_playlist_create,
            &serde_json::json!({ "name": name, "description": description }),
        )
    }

    fn playlist_add_tracks(&self, playlist_id: i64, track_ids: &[i64]) -> Result<Value, String> {
        appel(
            host_playlist_add_tracks,
            &serde_json::json!({ "playlist_id": playlist_id, "track_ids": track_ids }),
        )
    }

    fn streaming_playlists(&self, service: &str) -> Result<Value, String> {
        appel(
            host_streaming_playlists,
            &serde_json::json!({ "service": service }),
        )
    }

    fn streaming_playlist_tracks(&self, service: &str, playlist_id: &str) -> Result<Value, String> {
        appel(
            host_streaming_playlist_tracks,
            &serde_json::json!({ "service": service, "playlist_id": playlist_id }),
        )
    }

    fn streaming_playlist_create(
        &self,
        service: &str,
        name: &str,
        description: Option<&str>,
    ) -> Result<Value, String> {
        appel(
            host_streaming_playlist_create,
            &serde_json::json!({
                "service": service,
                "name": name,
                "description": description,
            }),
        )
    }

    fn streaming_playlist_add_tracks(
        &self,
        service: &str,
        playlist_id: &str,
        track_ids: &[String],
    ) -> Result<Value, String> {
        appel(
            host_streaming_playlist_add_tracks,
            &serde_json::json!({
                "service": service,
                "playlist_id": playlist_id,
                "track_ids": track_ids,
            }),
        )
    }

    fn streaming_match_track(
        &self,
        service: &str,
        title: &str,
        artist: &str,
        isrc: &str,
        duration_ms: u64,
    ) -> Result<Value, String> {
        appel(
            host_streaming_match_track,
            &serde_json::json!({
                "service": service,
                "title": title,
                "artist": artist,
                "isrc": isrc,
                "duration_ms": duration_ms,
            }),
        )
    }

    fn library_match_track(
        &self,
        title: &str,
        artist: &str,
        isrc: &str,
        duration_ms: u64,
    ) -> Result<Value, String> {
        appel(
            host_library_match_track,
            &serde_json::json!({
                "title": title,
                "artist": artist,
                "isrc": isrc,
                "duration_ms": duration_ms,
            }),
        )
    }

    fn kv_get(&self, key: &str) -> Result<Value, String> {
        appel(host_kv_get, &serde_json::json!({ "key": key }))
    }

    fn kv_set(&self, key: &str, value: &Value) -> Result<Value, String> {
        appel(
            host_kv_set,
            &serde_json::json!({ "key": key, "value": value }),
        )
    }

    fn kv_list(&self, prefix: &str) -> Result<Value, String> {
        appel(host_kv_list, &serde_json::json!({ "prefix": prefix }))
    }
}
