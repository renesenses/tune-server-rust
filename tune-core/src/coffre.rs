//! Le COFFRE des secrets de l'utilisateur : une valeur chiffrée au repos.
//!
//! # Une seule conception pour tout le serveur
//!
//! Deux chantiers en avaient besoin en même temps : les clés d'API
//! personnelles (AcoustID, Discogs — chantier « Enrichissement MetaRust »,
//! #4805) et la clé du fournisseur d'IA (chantier « Assistant IA »). Ce module
//! reprend À L'IDENTIQUE la construction du coffre de l'Assistant IA
//! (`plugins/tune-assistant-ia/src/coffre.rs`, branche
//! `feat/assistant-ia-etape1-b209ia`) et la remonte dans `tune-core`, pour
//! qu'il n'y en ait qu'UNE : même fichier de clé maîtresse, même format de
//! valeur scellée (`{"v":1,"nonce":…,"ct":…}`), mêmes primitives. Le greffon
//! scelle désormais sous la clé partagée ; son ancienne clé (`assistant-ia.cle`)
//! ouvre encore ce qu'elle a scellé ([`Coffre::avec_ancien_fichier`]) et
//! [`Coffre::resceller`] fait passer ces valeurs sous la clé partagée.
//!
//! # Ce qui existait, et pourquoi ce n'est pas suffisant
//!
//! Les jetons des services (Qobuz, Tidal, Discogs…) vivent EN CLAIR dans la
//! table `settings`. Ce qui les protège, c'est [`crate::secrets::est_secret`] :
//! tout réglage dont le NOM porte `token`, `secret`, `api_key`… est masqué par
//! `GET /system/config`, retiré des exports et de la sauvegarde dans le nuage.
//! Ce mécanisme est gardé : les valeurs scellées vivent sous des noms qui le
//! déclenchent. Mais une copie de `tune.db` — une sauvegarde, un WAL recopié,
//! un paquet de support — emporterait ces clés. Une clé d'API personnelle se
//! révoque moins vite qu'un jeton de session : elle est donc en plus SCELLÉE.
//!
//! # Construction
//!
//! - une clé maîtresse de 256 bits, tirée du générateur du système, posée
//!   dans un FICHIER à côté de la base (droits 0600) — jamais dans la base
//!   elle-même : une copie de la base seule ne suffit plus ;
//! - XChaCha20-Poly1305 (les primitives de [`crate::secret_envelope`]), un
//!   nonce aléatoire de 192 bits par scellement.
//!
//! # Ce que cela ne protège pas
//!
//! Qui détient la machine détient le fichier ET la base : le chiffrement
//! protège la copie de la base, pas la machine. Si le fichier disparaît (base
//! déplacée sans lui), la valeur ne s'ouvre plus : l'écran le dit
//! (`illisible`) et demande de la ressaisir. Rien d'autre n'est perdu.
//!
//! # Jamais en clair dans un journal
//!
//! Aucune fonction d'ici n'écrit de journal avec la valeur, ni ne la met dans
//! un message d'erreur : les erreurs décrivent la PANNE (« clé maîtresse
//! absente »), jamais le contenu.

use std::io::Write;
use std::path::{Path, PathBuf};

use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use serde::{Deserialize, Serialize};

use crate::secret_envelope::{hex_decode, hex_encode};

/// Le nom du fichier de la clé maîtresse partagée du serveur.
pub const FICHIER_CLE: &str = "tune-secrets.cle";

/// La forme d'une valeur scellée. Identique à celle du greffon Assistant IA.
#[derive(Serialize, Deserialize)]
struct Scelle {
    v: u8,
    nonce: String,
    ct: String,
}

/// Un coffre : le chemin de sa clé maîtresse. Ne garde jamais la clé en
/// mémoire au-delà d'un appel.
#[derive(Debug, Clone)]
pub struct Coffre {
    chemin: PathBuf,
    /// L'ancienne clé maîtresse d'un greffon passé au coffre partagé
    /// ([`Coffre::avec_ancien_fichier`]) : elle OUVRE encore ce qu'elle a
    /// scellé, elle ne scelle plus rien.
    ancien: Option<PathBuf>,
}

impl Coffre {
    /// Le coffre partagé du serveur, dont la clé maîtresse vit dans `dossier`.
    pub fn new(dossier: &Path) -> Self {
        Self::avec_fichier(dossier, FICHIER_CLE)
    }

    /// Un coffre dont la clé maîtresse porte un autre nom — pour un greffon
    /// qui a déjà scellé des valeurs sous sa propre clé (`assistant-ia.cle`).
    pub fn avec_fichier(dossier: &Path, fichier: &str) -> Self {
        Self {
            chemin: dossier.join(fichier),
            ancien: None,
        }
    }

    /// Le même coffre, qui ouvre AUSSI ce qu'une ancienne clé maîtresse,
    /// `fichier`, posée dans le même dossier, a scellé.
    ///
    /// C'est le passage d'un greffon à son coffre propre (`assistant-ia.cle`)
    /// au coffre partagé : une valeur scellée avant la mise à jour reste
    /// lisible, sans rien demander à l'utilisateur ; tout NOUVEAU scellement
    /// part sous la clé partagée ; [`Coffre::resceller`] fait passer une
    /// valeur ancienne sous la clé partagée.
    pub fn avec_ancien_fichier(mut self, fichier: &str) -> Self {
        let dossier = self
            .chemin
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        self.ancien = Some(dossier.join(fichier));
        self
    }

    /// Le coffre d'un serveur dont la base est `db_path` : la clé maîtresse
    /// est posée À CÔTÉ de la base. Un chemin sans dossier parent (base
    /// PostgreSQL, chemin nu) retombe sur le dossier courant du serveur.
    pub fn a_cote_de_la_base(db_path: &str) -> Self {
        Self::new(&dossier_de_la_base(db_path))
    }

    /// Le coffre du serveur EN COURS, installé au démarrage
    /// ([`installer_le_dossier_du_serveur`]). `None` hors serveur (essais,
    /// outils) : l'appelant retombe alors sur la clé intégrée.
    pub fn du_serveur() -> Option<Self> {
        DOSSIER_DU_SERVEUR.get().map(|d| Self::new(d))
    }

    fn lire_la_cle(&self) -> Result<Option<[u8; 32]>, String> {
        lire_la_cle_de(&self.chemin)
    }

    fn creer_la_cle(&self) -> Result<[u8; 32], String> {
        creer_la_cle_de(&self.chemin)
    }

    /// Scelle `clair`. Crée la clé maîtresse au premier usage.
    pub fn sceller(&self, clair: &str) -> Result<String, String> {
        let cle = match self.lire_la_cle()? {
            Some(c) => c,
            None => self.creer_la_cle()?,
        };
        let mut nonce = [0u8; 24];
        getrandom::getrandom(&mut nonce).map_err(|e| e.to_string())?;
        let chiffre = XChaCha20Poly1305::new((&cle).into())
            .encrypt(XNonce::from_slice(&nonce), clair.as_bytes())
            .map_err(|_| "scellement impossible".to_string())?;
        serde_json::to_string(&Scelle {
            v: 1,
            nonce: hex_encode(&nonce),
            ct: hex_encode(&chiffre),
        })
        .map_err(|e| e.to_string())
    }

    /// Ouvre une valeur scellée par [`Coffre::sceller`] — ou, pour un coffre
    /// [`Coffre::avec_ancien_fichier`], par l'ancienne clé maîtresse.
    pub fn ouvrir(&self, scelle: &str) -> Result<String, String> {
        match ouvrir_avec(&self.chemin, scelle) {
            Ok(clair) => Ok(clair),
            // L'erreur rendue reste celle de la clé PRINCIPALE : c'est elle
            // que l'écran doit faire ressaisir.
            Err(e) => match &self.ancien {
                Some(ancien) => ouvrir_avec(ancien, scelle).map_err(|_| e),
                None => Err(e),
            },
        }
    }

    /// Fait passer une valeur scellée sous la clé principale.
    ///
    /// - `Ok(None)` : elle s'ouvre déjà avec la clé principale, rien à faire ;
    /// - `Ok(Some(nouvelle))` : elle s'ouvrait avec l'ancienne clé ; la
    ///   nouvelle valeur, scellée sous la clé principale, a été ROUVERTE et
    ///   rend le même texte. À l'appelant de la ranger ;
    /// - `Err` : elle ne s'ouvre avec aucune des deux. Rien n'est écrit, pas
    ///   même la clé maîtresse partagée.
    pub fn resceller(&self, scelle: &str) -> Result<Option<String>, String> {
        let erreur = match ouvrir_avec(&self.chemin, scelle) {
            Ok(_) => return Ok(None),
            Err(e) => e,
        };
        let Some(ancien) = &self.ancien else {
            return Err(erreur);
        };
        let clair = ouvrir_avec(ancien, scelle).map_err(|_| erreur)?;
        let nouvelle = self.sceller(&clair)?;
        if ouvrir_avec(&self.chemin, &nouvelle)? != clair {
            return Err("la valeur rescellée ne rend pas le même texte".into());
        }
        Ok(Some(nouvelle))
    }
}

fn lire_la_cle_de(chemin: &Path) -> Result<Option<[u8; 32]>, String> {
    match std::fs::read_to_string(chemin) {
        Ok(t) => {
            let octets = hex_decode(t.trim()).map_err(|_| "clé maîtresse illisible".to_string())?;
            let cle: [u8; 32] = octets
                .try_into()
                .map_err(|_| "clé maîtresse de longueur inattendue".to_string())?;
            Ok(Some(cle))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("clé maîtresse illisible : {}", e.kind())),
    }
}

fn creer_la_cle_de(chemin: &Path) -> Result<[u8; 32], String> {
    let mut cle = [0u8; 32];
    getrandom::getrandom(&mut cle).map_err(|e| e.to_string())?;
    if let Some(parent) = chemin.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut f = options.open(chemin).map_err(|e| e.to_string())?;
    f.write_all(hex_encode(&cle).as_bytes())
        .map_err(|e| e.to_string())?;
    Ok(cle)
}

/// Ouvre `scelle` avec la clé maîtresse du fichier `chemin`.
fn ouvrir_avec(chemin: &Path, scelle: &str) -> Result<String, String> {
    let s: Scelle =
        serde_json::from_str(scelle).map_err(|_| "valeur scellée illisible".to_string())?;
    if s.v != 1 {
        return Err(format!("version de scellement inconnue : {}", s.v));
    }
    let cle = lire_la_cle_de(chemin)?.ok_or_else(|| "clé maîtresse absente".to_string())?;
    let nonce = hex_decode(&s.nonce).map_err(|_| "nonce illisible".to_string())?;
    if nonce.len() != 24 {
        return Err("nonce de longueur inattendue".into());
    }
    let ct = hex_decode(&s.ct).map_err(|_| "valeur scellée illisible".to_string())?;
    let clair = XChaCha20Poly1305::new((&cle).into())
        .decrypt(XNonce::from_slice(&nonce), ct.as_slice())
        .map_err(|_| "la valeur ne s'ouvre pas avec cette clé maîtresse".to_string())?;
    String::from_utf8(clair).map_err(|_| "valeur ouverte non UTF-8".to_string())
}

/// Le dossier du coffre du serveur, posé une fois au démarrage : les passes
/// de `tune-core` qui ne voient pas la configuration (portraits d'artistes)
/// y trouvent la clé personnelle sans qu'on leur passe un chemin.
static DOSSIER_DU_SERVEUR: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// Le dossier qui porte la base `db_path` (« . » pour un chemin nu).
pub fn dossier_de_la_base(db_path: &str) -> PathBuf {
    Path::new(db_path)
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Installe le dossier du coffre du serveur. Seul le premier appel compte.
pub fn installer_le_dossier_du_serveur(dossier: PathBuf) {
    let _ = DOSSIER_DU_SERVEUR.set(dossier);
}

/// Les quatre derniers caractères d'une clé, seuls montrés à l'écran.
///
/// Une clé de quatre caractères ou moins n'en montre AUCUN : la montrer
/// reviendrait à la rendre en clair.
pub fn fin_de_cle(cle: &str) -> String {
    let car: Vec<char> = cle.trim().chars().collect();
    if car.len() <= 4 {
        return String::new();
    }
    car[car.len() - 4..].iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sceller_puis_ouvrir_rend_la_cle_et_le_scelle_ne_la_contient_pas() {
        let dir = tempfile::tempdir().unwrap();
        let coffre = Coffre::new(dir.path());
        let scelle = coffre.sceller("cle-acoustid-SECRETE-abcd").unwrap();
        assert!(!scelle.contains("SECRETE"), "{scelle}");
        assert_eq!(coffre.ouvrir(&scelle).unwrap(), "cle-acoustid-SECRETE-abcd");
        // Deux scellements de la même clé ne se ressemblent pas (nonce).
        assert_ne!(scelle, coffre.sceller("cle-acoustid-SECRETE-abcd").unwrap());
    }

    #[test]
    fn sans_le_fichier_la_valeur_ne_s_ouvre_plus() {
        let dir = tempfile::tempdir().unwrap();
        let scelle = Coffre::new(dir.path()).sceller("cle").unwrap();
        let autre = tempfile::tempdir().unwrap();
        let err = Coffre::new(autre.path()).ouvrir(&scelle).unwrap_err();
        assert!(!err.contains("cle\""), "{err}");
    }

    /// Le format est celui du greffon Assistant IA : une valeur scellée par
    /// lui s'ouvre ici avec le même fichier, et inversement.
    #[test]
    fn le_coffre_du_greffon_ia_s_ouvre_avec_son_fichier() {
        let dir = tempfile::tempdir().unwrap();
        let ia = Coffre::avec_fichier(dir.path(), "assistant-ia.cle");
        let scelle = ia.sceller("sk-ia-1234").unwrap();
        let v: serde_json::Value = serde_json::from_str(&scelle).unwrap();
        assert_eq!(v["v"], 1);
        assert!(v["nonce"].as_str().unwrap().len() == 48);
        assert_eq!(
            Coffre::avec_fichier(dir.path(), "assistant-ia.cle")
                .ouvrir(&scelle)
                .unwrap(),
            "sk-ia-1234"
        );
        // Le coffre partagé a SA clé : il n'ouvre pas celle du greffon.
        assert!(Coffre::new(dir.path()).ouvrir(&scelle).is_err());
    }

    /// Le passage d'un greffon au coffre partagé : une valeur scellée sous
    /// l'ancienne clé s'ouvre encore, se rescelle sous la clé partagée, et le
    /// nouveau scellement ne dépend plus de l'ancienne clé.
    #[test]
    fn l_ancienne_cle_ouvre_encore_et_la_valeur_passe_sous_la_cle_partagee() {
        let dir = tempfile::tempdir().unwrap();
        let ancien = Coffre::avec_fichier(dir.path(), "assistant-ia.cle")
            .sceller("sk-ia-AVANT-1234")
            .unwrap();
        let coffre = Coffre::new(dir.path()).avec_ancien_fichier("assistant-ia.cle");
        assert_eq!(coffre.ouvrir(&ancien).unwrap(), "sk-ia-AVANT-1234");

        let nouvelle = coffre.resceller(&ancien).unwrap().expect("à resceller");
        assert!(!nouvelle.contains("AVANT"), "{nouvelle}");
        assert_eq!(
            Coffre::new(dir.path()).ouvrir(&nouvelle).unwrap(),
            "sk-ia-AVANT-1234"
        );
        assert_eq!(coffre.resceller(&nouvelle).unwrap(), None, "déjà passée");
        // Un nouveau scellement part sous la clé partagée.
        let neuf = coffre.sceller("sk-ia-NEUF").unwrap();
        assert_eq!(Coffre::new(dir.path()).ouvrir(&neuf).unwrap(), "sk-ia-NEUF");
        assert!(
            Coffre::avec_fichier(dir.path(), "assistant-ia.cle")
                .ouvrir(&neuf)
                .is_err()
        );
    }

    /// Une valeur qu'aucune des deux clés n'ouvre ne crée rien et rend une
    /// erreur qui ne contient pas la valeur.
    #[test]
    fn resceller_une_valeur_illisible_n_ecrit_rien() {
        let ailleurs = tempfile::tempdir().unwrap();
        let etrangere = Coffre::avec_fichier(ailleurs.path(), "assistant-ia.cle")
            .sceller("sk-ia-SECRET")
            .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let coffre = Coffre::new(dir.path()).avec_ancien_fichier("assistant-ia.cle");
        let err = coffre.resceller(&etrangere).unwrap_err();
        assert!(!err.contains("SECRET"), "{err}");
        assert!(coffre.ouvrir(&etrangere).is_err());
        assert!(!dir.path().join(FICHIER_CLE).exists());
    }

    #[cfg(unix)]
    #[test]
    fn le_fichier_n_est_lisible_que_par_son_proprietaire() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        Coffre::new(dir.path()).sceller("x").unwrap();
        let mode = std::fs::metadata(dir.path().join(FICHIER_CLE))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn la_fin_de_cle_ne_garde_que_quatre_caracteres() {
        assert_eq!(fin_de_cle("cle-1234567890wxyz"), "wxyz");
        assert_eq!(fin_de_cle("abcd"), "", "une clé courte ne se montre pas");
    }

    #[test]
    fn la_cle_vit_a_cote_de_la_base() {
        let c = Coffre::a_cote_de_la_base("/srv/tune/tune.db");
        assert_eq!(c.chemin, PathBuf::from("/srv/tune").join(FICHIER_CLE));
        let nu = Coffre::a_cote_de_la_base("tune.db");
        assert_eq!(nu.chemin, PathBuf::from(".").join(FICHIER_CLE));
    }
}
