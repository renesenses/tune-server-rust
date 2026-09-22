//! Le plan de transfert : ce que l'aperçu décide, ce que l'exécution consomme,
//! et ce qu'une reprise relit.
//!
//! Le plan est la SEULE mémoire du greffon. Il vit dans le stockage cloisonné
//! (`kv`), sous `transfert:{id}`, et il est réécrit après chaque écriture
//! réussie : c'est ce qui permet à un lot interrompu — coupure, service en
//! panne, serveur redémarré — de reprendre sans recréer ce qui existe déjà.

use serde::{Deserialize, Serialize};

/// D'où vient une playlist à transférer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Origine {
    /// Une playlist de la bibliothèque locale.
    Local { id: i64 },
    /// Une playlist chez un service de streaming.
    Service { service: String, id: String },
}

/// Où va le transfert.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Cible {
    /// La bibliothèque locale.
    Local,
    /// Un service de streaming, qui doit être authentifié ET savoir écrire.
    Service { service: String },
}

/// Le verdict de l'aperçu pour un titre. Il ne change JAMAIS à l'exécution :
/// l'exécution écrit ce que l'aperçu a annoncé, ou rien.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Statut {
    /// Appariée avec certitude (score au-dessus du seuil de l'hôte).
    Appariee,
    /// Trouvée, mais sans certitude. **Jamais écrite** : c'est le cas qui
    /// fabriquerait un doublon silencieux chez l'utilisateur.
    Approximative,
    /// Rien d'appariable. Rapportée avec sa raison, jamais inventée.
    Introuvable,
}

/// Les raisons, en CODES stables — jamais en phrases.
///
/// Le client web les traduit dans ses onze langues ; un serveur plus ancien
/// que son client, ou l'inverse, ne doit pas faire disparaître l'explication.
/// Même doctrine que `ModuleRefusal::code` côté serveur.
pub mod raison {
    /// Le service n'a rien rendu d'appariable pour ce titre.
    pub const AUCUN_RESULTAT: &str = "aucun_resultat";
    /// Un résultat existe mais sous le seuil d'acceptation de l'hôte.
    pub const APPARIEMENT_APPROXIMATIF: &str = "appariement_approximatif";
    /// Cible locale, source distante : l'interface hôte de la tranche 1 ne
    /// sait pas CHERCHER dans la bibliothèque locale (elle lit les playlists,
    /// pas le catalogue). On le dit, on n'invente pas de correspondance.
    pub const RECHERCHE_LOCALE_INDISPONIBLE: &str = "recherche_locale_indisponible";
    /// L'appariement lui-même a échoué (service injoignable, jeton expiré).
    pub const ERREUR_SERVICE: &str = "erreur_service";
}

/// Un titre de la playlist source, et son sort.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Ligne {
    pub titre: String,
    #[serde(default)]
    pub artiste: String,
    #[serde(default)]
    pub isrc: String,
    #[serde(default)]
    pub duree_ms: u64,
    pub statut: Statut,
    /// Le code de [`raison`], pour tout ce qui n'est pas [`Statut::Appariee`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raison: Option<String>,
    /// Le détail brut d'une erreur de service, quand il y en a un. Il complète
    /// la raison, il ne la remplace pas.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub score: Option<f64>,
    /// L'identifiant de la piste CHEZ LA CIBLE (service).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cible_piste: Option<String>,
    /// L'identifiant de la piste chez la cible, quand la cible est locale.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cible_piste_locale: Option<i64>,
    /// Déjà écrite chez la cible ? C'est le seul champ que l'exécution touche,
    /// et c'est lui qui empêche une reprise de dupliquer.
    #[serde(default)]
    pub ecrite: bool,
}

impl Ligne {
    /// Cette ligne est-elle à écrire au prochain passage ?
    pub fn reste_a_ecrire(&self) -> bool {
        self.statut == Statut::Appariee && !self.ecrite
    }
}

/// Une playlist source, sa cible, et le sort de chacun de ses titres.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Bloc {
    pub origine: Origine,
    /// Le nom lu chez la source.
    pub nom: String,
    /// Le nom que portera la playlist créée chez la cible.
    pub nom_cible: String,
    /// La playlist créée chez le service cible, une fois créée. Renseignée dès
    /// la création et persistée AUSSITÔT : une coupure juste après ne doit pas
    /// laisser une playlist orpheline qu'un second passage recréerait.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cible_playlist: Option<String>,
    /// Idem pour une cible locale.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cible_playlist_locale: Option<i64>,
    /// La dernière erreur rencontrée sur ce bloc, s'il en reste une.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub erreur: Option<String>,
    /// Le nombre de pistes que la CIBLE dit avoir acceptées, cumulé.
    ///
    /// Il n'est pas toujours égal au nombre de lignes marquées écrites : un
    /// service peut dédoublonner, une piste locale peut déjà être dans la
    /// playlist. Annoncer les deux est la seule façon honnête de le dire —
    /// un compteur qui ment est pire qu'un compteur absent (#3663).
    #[serde(default)]
    pub ajoutees: usize,
    pub lignes: Vec<Ligne>,
}

impl Bloc {
    pub fn comptes(&self) -> Comptes {
        let mut c = Comptes::default();
        for ligne in &self.lignes {
            match ligne.statut {
                Statut::Appariee => c.appariees += 1,
                Statut::Approximative => c.approximatives += 1,
                Statut::Introuvable => c.introuvables += 1,
            }
            if ligne.ecrite {
                c.ecrites += 1;
            }
        }
        c.total = self.lignes.len();
        c
    }
}

/// Le décompte d'un bloc ou d'un plan.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Comptes {
    pub total: usize,
    pub appariees: usize,
    pub approximatives: usize,
    pub introuvables: usize,
    pub ecrites: usize,
}

impl Comptes {
    pub fn cumuler(&mut self, autre: Comptes) {
        self.total += autre.total;
        self.appariees += autre.appariees;
        self.approximatives += autre.approximatives;
        self.introuvables += autre.introuvables;
        self.ecrites += autre.ecrites;
    }
}

/// Où en est le lot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Etat {
    /// Préparé, rien n'a été écrit chez l'utilisateur. Le seul état dans
    /// lequel un plan peut naître.
    Apercu,
    /// Entamé, mais tout n'est pas passé : une reprise a du travail.
    Partiel,
    /// Tout ce qui était appariable est écrit.
    Termine,
}

/// Le plan complet d'un transfert, simple ou par lot.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Plan {
    pub transfert_id: String,
    pub etat: Etat,
    pub cible: Cible,
    pub blocs: Vec<Bloc>,
}

impl Plan {
    /// La clé de ce plan dans le stockage cloisonné du greffon.
    pub fn cle(transfert_id: &str) -> String {
        format!("{PREFIXE_TRANSFERT}{transfert_id}")
    }

    pub fn comptes(&self) -> Comptes {
        let mut total = Comptes::default();
        for bloc in &self.blocs {
            total.cumuler(bloc.comptes());
        }
        total
    }

    /// Reste-t-il quelque chose à écrire ?
    ///
    /// La question porte sur les TITRES, pas sur les playlists cibles : un
    /// bloc dont aucun titre n'est appariable n'a rien à faire créer, et
    /// prétendre qu'il reste du travail ferait tourner une reprise en rond.
    pub fn reste_du_travail(&self) -> bool {
        self.blocs
            .iter()
            .any(|b| b.lignes.iter().any(Ligne::reste_a_ecrire))
    }
}

/// Le préfixe des plans dans le `kv`. Les clés du greffon lui sont propres :
/// l'hôte y ajoute encore `plugin_kv:{id}:`, que le greffon n'a pas à connaître.
pub const PREFIXE_TRANSFERT: &str = "transfert:";

/// La clé du compteur qui numérote les transferts.
///
/// Un greffon wasm n'a ni horloge ni aléa : le numéro vient d'un compteur
/// persistant, ce qui rend les identifiants — et donc les essais — stables.
pub const CLE_COMPTEUR: &str = "compteur";
