//! La surface de l'hôte, vue du greffon (#4716 côté serveur).
//!
//! Un trait, et non des appels directs aux imports `"tune"` : le moteur de
//! transfert est ainsi du Rust ordinaire, testable nativement contre un hôte de
//! banc qui COMPTE ses écritures. C'est cette instrumentation qui prouve que
//! l'aperçu n'écrit rien — une promesse qu'on ne peut pas tenir en la lisant.
//!
//! **Il n'y a aucune capacité de suppression ici, et il ne doit jamais y en
//! avoir** : l'interface hôte de la tranche 1 n'en expose pas, et une capacité
//! absente est la seule garde qu'on ne contourne pas.

use serde_json::Value;

/// Ce que rend une capacité de l'hôte : du JSON, ou le message d'un refus.
pub type Reponse = Result<Value, String>;

/// Les capacités que le convertisseur emprunte à l'hôte.
///
/// Chaque méthode correspond exactement à un import `"tune"` de la tranche 1,
/// avec les mêmes noms de champs. Les méthodes qui ÉCRIVENT sont regroupées et
/// signalées : ce sont les seules que l'aperçu ne doit jamais appeler.
pub trait Hote {
    /// Journal de diagnostic (toujours permis, aucune permission requise).
    fn journal(&self, niveau: &str, message: &str);

    // --- lecture -----------------------------------------------------------

    /// `playlists` — les playlists locales du profil actif.
    fn playlists_locales(&self, limite: i64, decalage: i64) -> Reponse;
    /// `playlists` — les pistes d'une playlist locale.
    fn pistes_locales(&self, playlist_id: i64) -> Reponse;
    /// `streaming` — les services authentifiés, et s'ils savent écrire.
    fn services(&self) -> Reponse;
    /// `streaming` — les playlists de l'utilisateur chez un service.
    fn playlists_du_service(&self, service: &str) -> Reponse;
    /// `streaming` — les pistes d'une playlist d'un service.
    fn pistes_du_service(&self, service: &str, playlist_id: &str) -> Reponse;
    /// `streaming` — apparier un titre chez un service. **Lecture seule** :
    /// l'hôte fait une recherche, il n'écrit rien.
    fn apparier(
        &self,
        service: &str,
        titre: &str,
        artiste: &str,
        isrc: &str,
        duree_ms: u64,
    ) -> Reponse;

    // --- état du greffon ----------------------------------------------------

    /// `kv` — lire l'état cloisonné du greffon.
    fn kv_lire(&self, cle: &str) -> Reponse;
    /// `kv` — écrire l'état cloisonné du greffon. Ce n'est PAS une écriture
    /// dans la bibliothèque ni chez un service : c'est le carnet du greffon,
    /// et c'est ce qui rend un lot interrompu reprenable.
    fn kv_ecrire(&self, cle: &str, valeur: &Value) -> Reponse;
    /// `kv` — lister les clés du greffon commençant par `prefixe`.
    fn kv_lister(&self, prefixe: &str) -> Reponse;

    // --- ÉCRITURES ----------------------------------------------------------
    //
    // 🔴 Les quatre seules méthodes qui modifient la bibliothèque ou un
    // service. L'aperçu n'en appelle AUCUNE ; l'exécution ne les appelle
    // qu'après un accord explicite. Aucune d'elles n'efface quoi que ce soit.

    /// `playlists` — créer une playlist locale.
    fn creer_playlist_locale(&self, nom: &str, description: Option<&str>) -> Reponse;
    /// `playlists` — AJOUTER des pistes à une playlist locale.
    fn ajouter_pistes_locales(&self, playlist_id: i64, pistes: &[i64]) -> Reponse;
    /// `streaming` — créer une playlist chez un service.
    fn creer_playlist_chez_le_service(
        &self,
        service: &str,
        nom: &str,
        description: Option<&str>,
    ) -> Reponse;
    /// `streaming` — AJOUTER des pistes à une playlist d'un service.
    fn ajouter_pistes_chez_le_service(
        &self,
        service: &str,
        playlist_id: &str,
        pistes: &[String],
    ) -> Reponse;
}

/// Traduire la réponse d'un import hôte en [`Reponse`].
///
/// L'hôte ne piège pas sur une erreur logique : il rend `{"error": "…"}` pour
/// que le greffon puisse la traiter (permission refusée, service injoignable,
/// playlist introuvable). La confondre avec un succès ferait un rapport qui
/// ment — et, pire, un lot « repris » qui recommencerait tout.
pub fn verdict(valeur: Value) -> Reponse {
    if let Some(message) = valeur.get("error") {
        let texte = message
            .as_str()
            .map(str::to_string)
            .unwrap_or_else(|| message.to_string());
        // La permission refusée mérite son propre mot : elle ne se répare pas
        // en réessayant, elle se répare dans le manifeste.
        if let Some(permission) = valeur.get("permission").and_then(Value::as_str) {
            return Err(format!("{texte} ({permission})"));
        }
        return Err(texte);
    }
    Ok(valeur)
}
