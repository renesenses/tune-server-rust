//! Le socle « source distante » (#8189, étape 2).
//!
//! Sur un téléphone, rien ne se monte : pas de `mount.cifs`, pas de
//! `mount_smbfs`. Le moteur lit donc lui-même les fichiers d'un NAS, par un
//! protocole en espace utilisateur ([`SystemeDistant`] : WebDAV, SMB, NFS
//! viendront aux étapes 3, 4 et 6), et sans les copier.
//!
//! ## Le chemin virtuel
//!
//! Un fichier distant est désigné par `tune-src://<source>/<chemin>` : c'est
//! ce chemin qui entre en base dans `tracks.file_path`. Il suit la voie déjà
//! tracée par les images ISO (#5299) : tout consommateur de chemin de piste
//! passe par les points d'entrée de [`crate::audio::iso9660`]
//! (`est_chemin_virtuel`, `ouvrir_fichier`, `taille_et_mtime`,
//! `source_symphonia`, `lire_si_virtuel`, `chemin_de_pochette`), qui
//! reconnaissent aussi ce préfixe. Lecture, étiquettes, empreinte et
//! pochettes suivent sans réécrire un seul décodeur.
//!
//! ## Le registre
//!
//! Une source configurée est enregistrée ici par son identifiant
//! ([`enregistrer`]). Une source absente du registre (type pas encore pris en
//! charge, identifiants illisibles) rend ses pistes INDISPONIBLES, pas
//! supprimées : `taille_et_mtime` rend `None`, et le parcours protège la
//! racine (`missing_dirs`).

pub mod identifiants;
pub mod jeton;
pub mod lecteur;
pub mod memoire;
pub mod parcours;
pub mod systeme;

#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};

pub use lecteur::LecteurDistant;
pub use parcours::PointDeReprise;
pub use systeme::{Entree, SystemeDistant};

/// Le préfixe des chemins virtuels de source distante.
pub const PREFIXE: &str = "tune-src://";

/// Un identifiant de source valide : 1 à 64 caractères `[A-Za-z0-9_-]`.
pub fn identifiant_valide(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Coupe un chemin virtuel en (source, chemin interne), ou `None`.
///
/// Le chemin interne est rendu sans `/` au bord, séparé par `/` (un `\`
/// glissé par une renormalisation Windows est accepté), et refusé s'il porte
/// un composant `..` : aucun chemin ne sort de la racine de la source.
pub fn decouper(chemin: &str) -> Option<(String, String)> {
    let reste = chemin
        .strip_prefix(PREFIXE)
        .or_else(|| chemin.strip_prefix("tune-src:\\\\"))?;
    let reste = reste.replace('\\', "/");
    let (id, interne) = match reste.split_once('/') {
        Some((id, i)) => (id.to_string(), i.trim_matches('/').to_string()),
        None => (reste.clone(), String::new()),
    };
    if !identifiant_valide(&id) {
        return None;
    }
    if interne.split('/').any(|c| c == "..") {
        return None;
    }
    Some((id, interne))
}

/// Le chemin virtuel d'un fichier `interne` de la source `id`.
pub fn chemin_distant(id: &str, interne: &str) -> String {
    let interne = interne.trim_matches('/');
    if interne.is_empty() {
        format!("{PREFIXE}{id}")
    } else {
        format!("{PREFIXE}{id}/{interne}")
    }
}

/// Dit si `chemin` désigne un fichier d'une source distante.
pub fn est_chemin_distant(chemin: &str) -> bool {
    decouper(chemin).is_some()
}

// ─── Le registre ────────────────────────────────────────────────────────

fn registre() -> &'static RwLock<HashMap<String, Arc<dyn SystemeDistant>>> {
    static R: std::sync::OnceLock<RwLock<HashMap<String, Arc<dyn SystemeDistant>>>> =
        std::sync::OnceLock::new();
    R.get_or_init(|| RwLock::new(HashMap::new()))
}

/// Enregistre (ou remplace) la source `id`.
pub fn enregistrer(id: &str, systeme: Arc<dyn SystemeDistant>) {
    registre()
        .write()
        .expect("registre des sources")
        .insert(id.to_string(), systeme);
    lecteur::oublier_les_blocs(id);
}

/// Retire la source `id` : ses pistes deviennent indisponibles.
pub fn retirer(id: &str) {
    registre().write().expect("registre des sources").remove(id);
    lecteur::oublier_les_blocs(id);
    reprises().lock().expect("reprises").remove(id);
    if let Ok(mut g) = persistances().write() {
        g.remove(id);
    }
}

/// La source `id`, si elle est enregistrée.
pub fn obtenir(id: &str) -> Option<Arc<dyn SystemeDistant>> {
    registre().read().ok()?.get(id).cloned()
}

/// Les identifiants des sources enregistrées, triés.
pub fn identifiants_enregistres() -> Vec<String> {
    let mut ids: Vec<String> = registre()
        .read()
        .map(|r| r.keys().cloned().collect())
        .unwrap_or_default();
    ids.sort();
    ids
}

/// Le réseau a changé (FFI `tune_server_network_changed`) : chaque source
/// oublie ses connexions.
pub fn reseau_change() {
    let sources: Vec<Arc<dyn SystemeDistant>> = registre()
        .read()
        .map(|r| r.values().cloned().collect())
        .unwrap_or_default();
    for s in sources {
        s.reseau_change();
    }
    for id in identifiants_enregistres() {
        lecteur::oublier_les_blocs(&id);
    }
}

fn source_de(chemin: &str) -> io::Result<(Arc<dyn SystemeDistant>, String, String)> {
    let (id, interne) = decouper(chemin)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "chemin distant invalide"))?;
    let systeme = obtenir(&id).ok_or_else(|| {
        io::Error::new(io::ErrorKind::NotConnected, "source réseau non disponible")
    })?;
    Ok((systeme, id, interne))
}

// ─── Les fabriques, par type de source ─────────────────────────────────

/// Ce qui construit le système d'une source configurée, à partir de sa ligne
/// et de ses identifiants (déjà ouverts).
pub type Fabrique = Arc<
    dyn Fn(
            &crate::db::sources_reseau::SourceReseau,
            Option<&identifiants::Identifiants>,
        ) -> io::Result<Arc<dyn SystemeDistant>>
        + Send
        + Sync,
>;

fn fabriques() -> &'static RwLock<HashMap<String, Fabrique>> {
    static F: std::sync::OnceLock<RwLock<HashMap<String, Fabrique>>> = std::sync::OnceLock::new();
    F.get_or_init(|| RwLock::new(HashMap::new()))
}

/// Déclare le protocole `type_source` (`webdav`, `smb`, `nfs`). Aucun n'est
/// déclaré à l'étape 2 : seules les épreuves déclarent `memoire`.
pub fn installer_la_fabrique(type_source: &str, f: Fabrique) {
    fabriques()
        .write()
        .expect("fabriques")
        .insert(type_source.to_string(), f);
}

pub fn fabrique(type_source: &str) -> Option<Fabrique> {
    fabriques().read().ok()?.get(type_source).cloned()
}

/// Les types de sources que ce serveur sait ouvrir, triés (annoncés par
/// `GET /system/capabilities`).
pub fn types_pris_en_charge() -> Vec<String> {
    let mut t: Vec<String> = fabriques()
        .read()
        .map(|f| f.keys().cloned().collect())
        .unwrap_or_default();
    t.sort();
    t
}

// ─── Les points d'entrée des consommateurs (appelés par `iso9660`) ─────

/// Ouvre le fichier distant `chemin` (virtuel).
pub fn ouvrir(chemin: &str) -> io::Result<LecteurDistant> {
    let (systeme, id, interne) = source_de(chemin)?;
    LecteurDistant::ouvrir_partage(systeme, Some(&id), &interne)
}

/// Taille et date d'un fichier distant ; `None` s'il est absent ou que la
/// source est injoignable.
pub fn stat(chemin: &str) -> Option<(u64, Option<std::time::SystemTime>)> {
    let (systeme, _, interne) = source_de(chemin).ok()?;
    let e = systeme.stat(&interne).ok()?;
    if e.dossier {
        return None;
    }
    let date = e
        .mtime
        .and_then(|s| u64::try_from(s).ok())
        .map(|s| std::time::UNIX_EPOCH + std::time::Duration::from_secs(s));
    Some((e.taille, date))
}

/// Lit en entier (au plus `plafond` octets) un fichier distant.
pub fn lire_entier(chemin: &str, plafond: u64) -> io::Result<Vec<u8>> {
    use std::io::Read;
    let mut l = ouvrir(chemin)?;
    if l.taille() > plafond {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "fichier trop gros pour être lu en entier",
        ));
    }
    let mut v = Vec::with_capacity(l.taille() as usize);
    l.read_to_end(&mut v)?;
    Ok(v)
}

/// La pochette du dossier distant d'une piste (`cover.jpg`, `folder.jpg`…),
/// cherchée dans son dossier puis dans ses parents jusqu'à la racine de la
/// source. Rend son chemin VIRTUEL.
pub fn chemin_de_pochette(chemin: &str) -> Option<PathBuf> {
    let (systeme, id, interne) = source_de(chemin).ok()?;
    let mut dossier = interne.rsplit_once('/').map(|(d, _)| d.to_string());
    loop {
        let d = dossier.clone().unwrap_or_default();
        if let Ok(entrees) = systeme.lister(&d) {
            for nom in crate::audio::iso9660::NOMS_DE_POCHETTE {
                if let Some(e) = entrees
                    .iter()
                    .find(|e| !e.dossier && e.nom.eq_ignore_ascii_case(nom))
                {
                    let interne = if d.is_empty() {
                        e.nom.clone()
                    } else {
                        format!("{d}/{}", e.nom)
                    };
                    return Some(PathBuf::from(chemin_distant(&id, &interne)));
                }
            }
        }
        match dossier {
            Some(d) if !d.is_empty() => {
                dossier = d.rsplit_once('/').map(|(p, _)| p.to_string());
            }
            _ => return None,
        }
    }
}

// ─── Le parcours de bibliothèque ───────────────────────────────────────

fn reprises() -> &'static Mutex<HashMap<String, PointDeReprise>> {
    static R: std::sync::OnceLock<Mutex<HashMap<String, PointDeReprise>>> =
        std::sync::OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Ce qui garde un point de reprise au-delà du processus (la table
/// `sources_reseau`, posée par le serveur). `None` : la reprise est effacée.
pub type Persistance = Arc<dyn Fn(&str, Option<&PointDeReprise>) + Send + Sync>;

fn persistances() -> &'static RwLock<HashMap<String, Persistance>> {
    static P: std::sync::OnceLock<RwLock<HashMap<String, Persistance>>> =
        std::sync::OnceLock::new();
    P.get_or_init(|| RwLock::new(HashMap::new()))
}

/// Installe la persistance des points de reprise de la source `id` (la base
/// du serveur qui l'a ouverte).
pub fn installer_la_persistance(id: &str, p: Persistance) {
    if let Ok(mut g) = persistances().write() {
        g.insert(id.to_string(), p);
    }
}

/// Rend à la source `id` un point de reprise relu de la base.
pub fn restaurer_la_reprise(id: &str, point: PointDeReprise) {
    reprises()
        .lock()
        .expect("reprises")
        .insert(id.to_string(), point);
}

/// Le point de reprise de la source `id` (épreuves, état à l'écran).
pub fn reprise_de(id: &str) -> Option<PointDeReprise> {
    reprises().lock().ok()?.get(id).cloned()
}

/// Tentatives par dossier pendant un parcours de bibliothèque.
pub const TENTATIVES_PAR_DOSSIER: u32 = 3;

/// Liste les fichiers audio sous la racine virtuelle `racine`
/// (`tune-src://<id>[/<dossier>]`), en reprenant un parcours interrompu.
/// Rend leurs chemins VIRTUELS, dans l'ordre du parcours.
///
/// Appelée par `scanner::walker` pour une racine distante. Une erreur dit que
/// la racine est illisible : le parcours la protège de la purge.
pub fn parcourir_la_racine(
    racine: &str,
    garder: &dyn Fn(&str) -> bool,
    arret: &dyn Fn() -> bool,
    attente: std::time::Duration,
) -> io::Result<Vec<String>> {
    let (systeme, id, interne) = source_de(racine)?;
    let mut point = reprise_de(&id).unwrap_or_default();
    let reglages = parcours::Reglages {
        garder,
        arret,
        tentatives: TENTATIVES_PAR_DOSSIER,
        attente,
    };
    let resultat = parcours::parcourir(systeme.as_ref(), &interne, &mut point, &reglages);
    let p = persistances().read().ok().and_then(|g| g.get(&id).cloned());
    match resultat {
        Ok(fait) => {
            reprises().lock().expect("reprises").remove(&id);
            if let Some(p) = p {
                p(&id, None);
            }
            tracing::info!(
                source = %id,
                fichiers = fait.fichiers.len(),
                dossiers_listes = fait.dossiers_listes,
                dossiers_repris = fait.dossiers_repris,
                "source_distante_parcourue"
            );
            Ok(fait
                .fichiers
                .into_iter()
                .map(|f| chemin_distant(&id, &f.chemin))
                .collect())
        }
        Err(e) => {
            tracing::warn!(
                source = %id,
                dossiers_retenus = point.dossiers_finis.len(),
                error = %e,
                "source_distante_parcours_interrompu"
            );
            if let Some(p) = p {
                p(&id, Some(&point));
            }
            reprises()
                .lock()
                .expect("reprises")
                .insert(id.clone(), point);
            Err(e)
        }
    }
}
