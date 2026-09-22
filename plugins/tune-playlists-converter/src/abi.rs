//! La couche d'ABI wasm : les exports que l'hôte résout, et les imports
//! `"tune"` qu'il installe sur le `Linker`.
//!
//! Elle ne décide de RIEN. Tout ce qui juge — appariement, aperçu, reprise —
//! vit dans [`crate::moteur`], en Rust ordinaire, et ses essais tournent
//! nativement. Ici il n'y a que du marshaling : allouer, écrire, appeler,
//! lire, libérer. C'est voulu : c'est la seule partie que la porte
//! d'intégration ne peut pas exécuter.
//!
//! Le protocole est celui de `tune-plugin-runtime-wasm` :
//!
//! * l'hôte appelle `alloc(len)`, écrit son JSON, appelle
//!   `plugin_dispatch(ptr, len)` ;
//! * le greffon rend un `i64` qui empaquette `(ptr << 32) | len` ; l'hôte lit
//!   ce tampon puis appelle `dealloc` dessus ;
//! * un import hôte suit la même convention, en sens inverse : le greffon
//!   passe `(ptr, len)`, l'hôte alloue sa réponse par NOTRE `alloc`, et c'est
//!   au greffon de la libérer.

#![allow(clippy::missing_safety_doc)]

use serde_json::Value;

use crate::hote::{Hote, Reponse, verdict};

// ---------------------------------------------------------------------------
// Mémoire partagée avec l'hôte
// ---------------------------------------------------------------------------

/// Réserver `len` octets que l'hôte va remplir. Le tampon est oublié : c'est
/// `dealloc` qui le rendra.
#[unsafe(no_mangle)]
pub extern "C" fn alloc(len: u32) -> u32 {
    let mut tampon = Vec::<u8>::with_capacity(len as usize);
    let pointeur = tampon.as_mut_ptr();
    core::mem::forget(tampon);
    pointeur as u32
}

/// Rendre un tampon alloué par [`alloc`].
///
/// # Safety
/// `pointeur`/`len` doivent venir d'un [`alloc`] non encore libéré.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dealloc(pointeur: u32, len: u32) {
    if pointeur == 0 || len == 0 {
        return;
    }
    unsafe {
        drop(Vec::from_raw_parts(
            pointeur as *mut u8,
            len as usize,
            len as usize,
        ));
    }
}

/// La version d'ABI parlée par ce greffon. Un écart ⇒ l'hôte refuse de le
/// charger (RFC §4).
#[unsafe(no_mangle)]
pub extern "C" fn abi_version() -> u32 {
    crate::ABI
}

/// Le point d'entrée des routes montées sous
/// `/api/v1/plugins/playlists-converter/…`.
///
/// # Safety
/// `pointeur`/`len` désignent le JSON de requête écrit par l'hôte.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn plugin_dispatch(pointeur: u32, len: u32) -> i64 {
    let entree = unsafe { core::slice::from_raw_parts(pointeur as *const u8, len as usize) };
    let requete: Value = serde_json::from_slice(entree).unwrap_or(Value::Null);
    let reponse = crate::routage::repondre(&HoteWasm, &requete);
    rendre(&reponse)
}

/// Sérialiser une valeur dans un tampon que l'hôte lira puis libérera.
fn rendre(valeur: &Value) -> i64 {
    let octets = serde_json::to_vec(valeur).unwrap_or_else(|_| b"{\"status\":500}".to_vec());
    let len = octets.len() as u32;
    let pointeur = alloc(len);
    unsafe {
        core::ptr::copy_nonoverlapping(octets.as_ptr(), pointeur as *mut u8, len as usize);
    }
    ((pointeur as i64) << 32) | (len as i64)
}

// ---------------------------------------------------------------------------
// Les imports de l'hôte (#4716)
// ---------------------------------------------------------------------------

#[link(wasm_import_module = "tune")]
unsafe extern "C" {
    fn host_log(pointeur: i32, len: i32);
    fn host_playlists_list(pointeur: i32, len: i32) -> i64;
    fn host_playlist_tracks(pointeur: i32, len: i32) -> i64;
    fn host_playlist_create(pointeur: i32, len: i32) -> i64;
    fn host_playlist_add_tracks(pointeur: i32, len: i32) -> i64;
    fn host_streaming_services(pointeur: i32, len: i32) -> i64;
    fn host_streaming_playlists(pointeur: i32, len: i32) -> i64;
    fn host_streaming_playlist_tracks(pointeur: i32, len: i32) -> i64;
    fn host_streaming_playlist_create(pointeur: i32, len: i32) -> i64;
    fn host_streaming_playlist_add_tracks(pointeur: i32, len: i32) -> i64;
    fn host_streaming_match_track(pointeur: i32, len: i32) -> i64;
    fn host_kv_get(pointeur: i32, len: i32) -> i64;
    fn host_kv_set(pointeur: i32, len: i32) -> i64;
    fn host_kv_list(pointeur: i32, len: i32) -> i64;
}

/// Appeler un import JSON de l'hôte.
fn appeler(import: unsafe extern "C" fn(i32, i32) -> i64, entree: &Value) -> Result<Value, String> {
    let octets = serde_json::to_vec(entree).map_err(|e| format!("requête hôte illisible : {e}"))?;
    let empaquete = unsafe { import(octets.as_ptr() as i32, octets.len() as i32) };
    let pointeur = (empaquete >> 32) as u32;
    let len = (empaquete & 0xFFFF_FFFF) as u32;
    if pointeur == 0 || len == 0 {
        return Err("réponse hôte vide".to_string());
    }
    // L'hôte a écrit dans un tampon de NOTRE allocateur : on le reprend, donc
    // on le libère — sinon chaque appariement fuite dans la mémoire linéaire,
    // et un lot de trois cents titres finit par heurter le plafond de 64 Mio.
    let tampon = unsafe { Vec::from_raw_parts(pointeur as *mut u8, len as usize, len as usize) };
    let valeur: Value =
        serde_json::from_slice(&tampon).map_err(|e| format!("réponse hôte illisible : {e}"))?;
    verdict(valeur)
}

/// L'hôte réel, vu du bac à sable.
pub struct HoteWasm;

impl Hote for HoteWasm {
    fn journal(&self, niveau: &str, message: &str) {
        let entree = serde_json::json!({ "level": niveau, "msg": message });
        if let Ok(octets) = serde_json::to_vec(&entree) {
            unsafe { host_log(octets.as_ptr() as i32, octets.len() as i32) };
        }
    }

    fn playlists_locales(&self, limite: i64, decalage: i64) -> Reponse {
        appeler(
            host_playlists_list,
            &serde_json::json!({ "limit": limite, "offset": decalage }),
        )
    }

    fn pistes_locales(&self, playlist_id: i64) -> Reponse {
        appeler(
            host_playlist_tracks,
            &serde_json::json!({ "playlist_id": playlist_id }),
        )
    }

    fn services(&self) -> Reponse {
        appeler(host_streaming_services, &serde_json::json!({}))
    }

    fn playlists_du_service(&self, service: &str) -> Reponse {
        appeler(
            host_streaming_playlists,
            &serde_json::json!({ "service": service }),
        )
    }

    fn pistes_du_service(&self, service: &str, playlist_id: &str) -> Reponse {
        appeler(
            host_streaming_playlist_tracks,
            &serde_json::json!({ "service": service, "playlist_id": playlist_id }),
        )
    }

    fn apparier(
        &self,
        service: &str,
        titre: &str,
        artiste: &str,
        isrc: &str,
        duree_ms: u64,
    ) -> Reponse {
        appeler(
            host_streaming_match_track,
            &serde_json::json!({
                "service": service,
                "title": titre,
                "artist": artiste,
                "isrc": isrc,
                "duration_ms": duree_ms,
            }),
        )
    }

    fn kv_lire(&self, cle: &str) -> Reponse {
        appeler(host_kv_get, &serde_json::json!({ "key": cle }))
    }

    fn kv_ecrire(&self, cle: &str, valeur: &Value) -> Reponse {
        appeler(
            host_kv_set,
            &serde_json::json!({ "key": cle, "value": valeur }),
        )
    }

    fn kv_lister(&self, prefixe: &str) -> Reponse {
        appeler(host_kv_list, &serde_json::json!({ "prefix": prefixe }))
    }

    fn creer_playlist_locale(&self, nom: &str, description: Option<&str>) -> Reponse {
        appeler(
            host_playlist_create,
            &serde_json::json!({ "name": nom, "description": description }),
        )
    }

    fn ajouter_pistes_locales(&self, playlist_id: i64, pistes: &[i64]) -> Reponse {
        appeler(
            host_playlist_add_tracks,
            &serde_json::json!({ "playlist_id": playlist_id, "track_ids": pistes }),
        )
    }

    fn creer_playlist_chez_le_service(
        &self,
        service: &str,
        nom: &str,
        description: Option<&str>,
    ) -> Reponse {
        appeler(
            host_streaming_playlist_create,
            &serde_json::json!({ "service": service, "name": nom, "description": description }),
        )
    }

    fn ajouter_pistes_chez_le_service(
        &self,
        service: &str,
        playlist_id: &str,
        pistes: &[String],
    ) -> Reponse {
        appeler(
            host_streaming_playlist_add_tracks,
            &serde_json::json!({
                "service": service,
                "playlist_id": playlist_id,
                "track_ids": pistes,
            }),
        )
    }
}
