//! Le contrat d'un système de fichiers distant (#8189, étape 2).
//!
//! Un protocole (WebDAV à l'étape 3, SMB à la 4, NFS à la 6) n'a que quatre
//! choses à savoir faire : lister un dossier, décrire un fichier, en lire une
//! PLAGE d'octets, et oublier ses connexions quand le réseau change. Tout le
//! reste — lecture à tampon, chemins virtuels, parcours reprenable, passerelle
//! HTTP, identifiants scellés — est commun et vit dans ce module.
//!
//! Le contrat est SYNCHRONE : ses appelants sont les décodeurs (symphonia), les
//! lecteurs d'étiquettes (lofty) et le parcours de bibliothèque, qui tournent
//! tous dans des fils bloquants (rayon, `spawn_blocking`). Un protocole bâti
//! sur tokio fait le pont lui-même (`Handle::block_on` depuis un fil hors
//! runtime), comme `staged_growth` le fait déjà.
//!
//! Les chemins sont RELATIFS à la racine de la source, séparés par `/`, sans
//! `/` initial ; `""` désigne la racine.
//!
//! Les erreurs suivent `std::io::ErrorKind`, que l'écran traduit :
//! - `NotConnected`, `TimedOut`, `ConnectionRefused`… : source injoignable ;
//! - `PermissionDenied` : identifiants refusés ;
//! - `NotFound` : le fichier ou le dossier n'existe pas (ou plus).

use std::io;

/// Une entrée de dossier, ou la description d'un fichier.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Entree {
    /// Le dernier composant du chemin (`01 Intro.flac`).
    pub nom: String,
    pub dossier: bool,
    /// Taille en octets (0 pour un dossier).
    pub taille: u64,
    /// Date de modification, secondes depuis Epoch, si le serveur la donne.
    pub mtime: Option<i64>,
    /// Étiquette de version (WebDAV `getetag`), si le serveur en donne une :
    /// le scan incrémental la compare en plus de la taille et de la date.
    pub etag: Option<String>,
}

/// Un système de fichiers distant, lu sans montage par le moteur.
pub trait SystemeDistant: Send + Sync {
    /// `webdav`, `smb`, `nfs` (ou `memoire` pour les épreuves).
    fn type_de_source(&self) -> &'static str;

    /// Le contenu d'un dossier, dans un ordre quelconque.
    fn lister(&self, dossier: &str) -> io::Result<Vec<Entree>>;

    /// La description d'un fichier ou d'un dossier.
    fn stat(&self, chemin: &str) -> io::Result<Entree>;

    /// Lit au plus `tampon.len()` octets à partir de `position`. Rend 0 à la
    /// fin du fichier. Une lecture courte n'est pas une erreur.
    fn lire_a(&self, chemin: &str, position: u64, tampon: &mut [u8]) -> io::Result<usize>;

    /// Le réseau a changé (Wi-Fi coupé, autre réseau) : oublier sessions et
    /// connexions, la prochaine lecture se reconnecte.
    fn reseau_change(&self) {}
}

/// Le motif d'une erreur, tel que l'écran le nomme (`{code, message, action}`
/// côté route).
pub fn code_d_erreur(e: &io::Error) -> &'static str {
    use io::ErrorKind as K;
    match e.kind() {
        K::PermissionDenied => "identifiants_refuses",
        K::NotFound => "introuvable",
        K::NotConnected
        | K::TimedOut
        | K::ConnectionRefused
        | K::ConnectionReset
        | K::ConnectionAborted
        | K::HostUnreachable
        | K::NetworkUnreachable
        | K::NetworkDown
        | K::BrokenPipe => "injoignable",
        _ => "erreur",
    }
}
