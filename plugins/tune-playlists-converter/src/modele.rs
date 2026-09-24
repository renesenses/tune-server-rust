//! Les objets que le greffon échange avec l'écran, et qu'il persiste.

use serde::{Deserialize, Serialize};

use crate::appariement::Raison;

/// Ce que l'écran demande : d'où, vers où, quelles playlists.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Demande {
    /// `"local"` pour la bibliothèque, sinon le nom d'un service authentifié.
    pub source_service: String,
    /// Le service d'arrivée. Voir `moteur` : la bibliothèque locale ne peut pas
    /// encore être une CIBLE, faute de capacité d'appariement local côté hôte.
    pub cible_service: String,
    /// Les identifiants source. Des entiers en texte pour `"local"`, les
    /// identifiants du service sinon. Plusieurs = le mode par lot.
    pub playlists: Vec<String>,
    /// Suffixe ajouté au nom de chaque playlist créée. Absent ⇒ le nom est
    /// repris **à l'identique**, ce que demande le ticket.
    #[serde(default)]
    pub suffixe_nom: Option<String>,
}

/// Un titre dont les trois critères concordent : il sera versé tel quel.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Appariee {
    pub source_titre: String,
    pub source_artiste: String,
    pub source_duree_ms: u64,
    pub cible_id: String,
    pub cible_titre: String,
    pub cible_artiste: String,
    pub cible_duree_ms: u64,
    pub score: f64,
}

/// Un titre qui ne sera pas transféré, et pourquoi.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Introuvable {
    pub source_titre: String,
    pub source_artiste: String,
    pub source_duree_ms: u64,
    pub raison: Raison,
}

/// L'état d'une playlist dans le lot.
///
/// Chaînes et non variantes nues : l'écran les lit, et un état ajouté plus tard
/// ne doit pas casser une réponse déjà écrite en base.
pub mod etat {
    /// Apparié, rien d'écrit. C'est l'état de tout ce qui sort d'un aperçu.
    pub const APERCU: &str = "apercu";
    /// Le transfert a commencé — playlist créée, versement en cours.
    pub const EN_COURS: &str = "en_cours";
    /// Tout ce qui devait être versé l'a été.
    pub const TERMINE: &str = "termine";
    /// Le versement s'est arrêté sur une erreur. Reprenable.
    pub const INTERROMPU: &str = "interrompu";
    /// Aucun titre apparié : il n'y a rien à créer, et on ne crée pas une
    /// playlist vide chez un service.
    pub const RIEN_A_TRANSFERER: &str = "rien_a_transferer";
}

/// Une playlist du lot : son aperçu, puis son avancement.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlaylistDuLot {
    /// Rang dans le lot. Sert de clé de stockage (`lot:{id}:pl:{rang}`).
    pub rang: usize,
    pub source_playlist_id: String,
    pub source_nom: String,
    pub cible_nom: String,
    /// Nombre de titres lus à la source.
    pub total: usize,
    pub appariees: Vec<Appariee>,
    pub introuvables: Vec<Introuvable>,
    /// L'identifiant de la playlist créée chez la cible. Présent ⇒ **elle
    /// existe déjà**, une reprise ne doit pas en créer une seconde.
    #[serde(default)]
    pub cible_playlist_id: Option<String>,
    /// Les identifiants CIBLE déjà versés. Une reprise ne repasse pas dessus.
    #[serde(default)]
    pub versees: Vec<String>,
    pub etat: String,
    #[serde(default)]
    pub erreur: Option<String>,
}

impl PlaylistDuLot {
    /// Ce qui reste à verser : les appariées qui ne sont pas déjà dans
    /// `versees`. C'est toute la reprise.
    pub fn restant_a_verser(&self) -> Vec<String> {
        self.appariees
            .iter()
            .map(|a| a.cible_id.clone())
            .filter(|id| !self.versees.contains(id))
            .collect()
    }
}

/// L'en-tête d'un lot, persisté seul pour rester petit.
///
/// Le stockage clé/valeur de l'hôte borne une valeur à 256 Kio : un lot de
/// trente playlists de trois cents titres n'y tiendrait pas d'un seul bloc.
/// Chaque playlist a donc sa propre clé, et l'en-tête ne porte que le
/// dénombrement.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnTeteLot {
    pub lot_id: String,
    pub source_service: String,
    pub cible_service: String,
    pub etat: String,
    /// Les rangs des playlists du lot, dans l'ordre demandé.
    pub rangs: Vec<usize>,
}

/// Un lot complet, en mémoire : l'en-tête et ses playlists.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Lot {
    pub lot_id: String,
    pub source_service: String,
    pub cible_service: String,
    pub etat: String,
    pub playlists: Vec<PlaylistDuLot>,
}

impl Lot {
    /// Le résumé que l'écran affiche avant de demander l'accord : ce qui sera
    /// créé, combien de titres appariés, combien manquent.
    pub fn resume(&self) -> serde_json::Value {
        let appariees: usize = self.playlists.iter().map(|p| p.appariees.len()).sum();
        let introuvables: usize = self.playlists.iter().map(|p| p.introuvables.len()).sum();
        let total: usize = self.playlists.iter().map(|p| p.total).sum();
        let versees: usize = self.playlists.iter().map(|p| p.versees.len()).sum();
        serde_json::json!({
            "lot_id": self.lot_id,
            "etat": self.etat,
            "playlists": self.playlists.len(),
            "titres": total,
            "appariees": appariees,
            "introuvables": introuvables,
            "versees": versees,
        })
    }
}
