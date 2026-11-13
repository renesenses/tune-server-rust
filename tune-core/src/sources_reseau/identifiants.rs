//! Les identifiants d'une source réseau : JAMAIS en clair (#8189).
//!
//! Ils sont scellés par le coffre partagé ([`crate::coffre`], une seule
//! conception pour tout le serveur, règle du 12/10). Sur un téléphone, la clé
//! maîtresse vient de l'application hôte (`tune_server_set_master_key`) :
//! iOS la range dans le trousseau, Android la chiffre par une clé du
//! Keystore. Elle n'est tenue qu'en mémoire et n'est jamais écrite sur le
//! disque : une copie de la base, ou du conteneur de l'application, ne suffit
//! pas à ouvrir un mot de passe.
//!
//! Sur un bureau, sans clé fournie, le coffre à fichier du serveur sert (clé
//! à côté de la base, droits 0600), comme pour les clés d'API personnelles.
//! Un serveur mobile SANS clé de l'hôte refuse d'enregistrer un identifiant
//! plutôt que d'écrire une clé sur le disque.
//!
//! Aucune fonction d'ici n'écrit le secret dans un journal ni dans une erreur.

use std::sync::RwLock;

use crate::coffre::Coffre;

static CLE_DE_L_HOTE: RwLock<Option<zeroize::Zeroizing<[u8; 32]>>> = RwLock::new(None);

/// Pose la clé maîtresse fournie par l'application hôte (32 octets).
pub fn installer_la_cle_de_l_hote(cle: [u8; 32]) {
    if let Ok(mut g) = CLE_DE_L_HOTE.write() {
        *g = Some(zeroize::Zeroizing::new(cle));
    }
}

/// Oublie la clé de l'hôte (arrêt du serveur, épreuves).
pub fn oublier_la_cle_de_l_hote() {
    if let Ok(mut g) = CLE_DE_L_HOTE.write() {
        *g = None;
    }
}

pub fn cle_de_l_hote_installee() -> bool {
    CLE_DE_L_HOTE.read().map(|g| g.is_some()).unwrap_or(false)
}

/// Un identifiant de compte. Le mot de passe est effacé à la libération et
/// n'apparaît dans aucun `Debug`.
#[derive(Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Identifiants {
    pub utilisateur: String,
    pub mot_de_passe: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub domaine: Option<String>,
}

impl Drop for Identifiants {
    fn drop(&mut self) {
        use zeroize::Zeroize;
        self.mot_de_passe.zeroize();
    }
}

impl std::fmt::Debug for Identifiants {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Identifiants")
            .field("utilisateur", &self.utilisateur)
            .field("mot_de_passe", &"<masqué>")
            .field("domaine", &self.domaine)
            .finish()
    }
}

/// Pourquoi un identifiant ne se scelle ou ne s'ouvre pas.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ErreurCoffre {
    /// Serveur mobile sans clé fournie par l'application.
    CleDeLHoteAbsente,
    /// Serveur de bureau sans coffre installé (essais, outils).
    CoffreAbsent,
    /// La valeur ne s'ouvre pas (clé changée, valeur abîmée).
    Illisible(String),
}

impl ErreurCoffre {
    pub fn code(&self) -> &'static str {
        match self {
            Self::CleDeLHoteAbsente => "cle_de_l_hote_absente",
            Self::CoffreAbsent => "coffre_absent",
            Self::Illisible(_) => "identifiants_illisibles",
        }
    }
}

/// Le coffre des sources : celui de la clé de l'hôte si elle est posée ;
/// sinon, hors mobile, le coffre à fichier du serveur.
fn coffre(mobile: bool) -> Result<Coffre, ErreurCoffre> {
    if let Some(cle) = CLE_DE_L_HOTE
        .read()
        .ok()
        .and_then(|g| g.as_ref().map(|c| **c))
    {
        return Ok(Coffre::en_memoire(cle));
    }
    if mobile {
        return Err(ErreurCoffre::CleDeLHoteAbsente);
    }
    Coffre::du_serveur().ok_or(ErreurCoffre::CoffreAbsent)
}

/// Scelle des identifiants pour la base.
pub fn sceller(ids: &Identifiants, mobile: bool) -> Result<String, ErreurCoffre> {
    let clair = zeroize::Zeroizing::new(
        serde_json::to_string(ids).map_err(|e| ErreurCoffre::Illisible(e.to_string()))?,
    );
    coffre(mobile)?
        .sceller(&clair)
        .map_err(ErreurCoffre::Illisible)
}

/// Ouvre des identifiants scellés par [`sceller`].
pub fn ouvrir(scelle: &str, mobile: bool) -> Result<Identifiants, ErreurCoffre> {
    let clair = zeroize::Zeroizing::new(
        coffre(mobile)?
            .ouvrir(scelle)
            .map_err(ErreurCoffre::Illisible)?,
    );
    serde_json::from_str(&clair).map_err(|_| ErreurCoffre::Illisible("format".into()))
}
