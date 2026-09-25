//! La surface hôte, vue du greffon.
//!
//! Un miroir EXACT des fonctions installées par la tranche 1 (#4716) sur le
//! module wasm `"tune"` : mêmes noms, mêmes clés JSON d'entrée, mêmes clés de
//! sortie. Rien n'est ajouté, et surtout rien n'est retiré de la contrainte —
//! **il n'y a aucune méthode de suppression ici, parce qu'il n'existe aucune
//! fonction hôte de suppression à appeler.**
//!
//! Pourquoi une abstraction plutôt que les `extern "C"` partout : le moteur du
//! transfert doit pouvoir être joué sur la machine de porte, en natif, contre
//! un double. C'est la seule façon de prouver qu'un aperçu n'écrit rien sans
//! écrire pour de vrai chez TIDAL ou Qobuz pour le vérifier.

use serde_json::Value;

/// Ce que le greffon peut demander à l'hôte.
///
/// Toutes les méthodes rendent le JSON de l'hôte tel quel. Une erreur logique
/// (`{"error": "…"}` côté hôte) remonte en `Err` : l'appelant décide s'il
/// s'arrête ou s'il note la raison et continue — ce que fait le moteur pour
/// une piste, et pas pour une playlist entière.
pub trait Hote {
    /// Journal de diagnostic. Toujours autorisé, aucune permission.
    fn journal(&self, niveau: &str, message: &str);

    /// L'heure de l'hôte, en millisecondes Unix (`host_now`, #4718). Toujours
    /// autorisée : un greffon wasm n'a pas d'horloge, et un snapshot doit être
    /// daté. `0` si l'hôte ne répond pas — un snapshot daté de 1970 se voit,
    /// un greffon qui s'arrête faute d'heure ne se voit pas.
    fn maintenant_ms(&self) -> u64;

    // -- permission `playlists` : la bibliothèque locale -------------------
    fn playlist_tracks(&self, playlist_id: i64) -> Result<Value, String>;
    /// Créer une playlist LOCALE (#4718 : restaurer un snapshot local). Jamais
    /// en effacer une : la capacité n'existe pas.
    fn playlist_create(&self, name: &str, description: Option<&str>) -> Result<Value, String>;
    /// AJOUTER des pistes à une playlist locale.
    fn playlist_add_tracks(&self, playlist_id: i64, track_ids: &[i64]) -> Result<Value, String>;

    // -- permission `streaming` : les services ------------------------------
    fn streaming_playlists(&self, service: &str) -> Result<Value, String>;
    fn streaming_playlist_tracks(&self, service: &str, playlist_id: &str) -> Result<Value, String>;
    fn streaming_playlist_create(
        &self,
        service: &str,
        name: &str,
        description: Option<&str>,
    ) -> Result<Value, String>;
    fn streaming_playlist_add_tracks(
        &self,
        service: &str,
        playlist_id: &str,
        track_ids: &[String],
    ) -> Result<Value, String>;
    fn streaming_match_track(
        &self,
        service: &str,
        title: &str,
        artist: &str,
        isrc: &str,
        duration_ms: u64,
    ) -> Result<Value, String>;

    // -- permission `library` : apparier DANS la bibliothèque locale ---------
    /// Même forme de réponse que [`Hote::streaming_match_track`] ; la piste
    /// porte son identifiant entier sous `track_id` (#4719 : un lien dont une
    /// extrémité est la bibliothèque locale).
    fn library_match_track(
        &self,
        title: &str,
        artist: &str,
        isrc: &str,
        duration_ms: u64,
    ) -> Result<Value, String>;

    // -- permission `kv` : l'état des lots, cloisonné par l'hôte ------------
    fn kv_get(&self, key: &str) -> Result<Value, String>;
    fn kv_set(&self, key: &str, value: &Value) -> Result<Value, String>;
    fn kv_list(&self, prefix: &str) -> Result<Value, String>;
}
