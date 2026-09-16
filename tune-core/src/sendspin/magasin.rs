//! Identite et PSK longue duree, dans un magasin prive local.
//!
//! Le dossier est choisi par le serveur, sous son repertoire de donnees.
//! Le verrou reste pris jusqu'au Drop. Ecrire/flusher un fichier temporaire,
//! puis le renommer, evite de publier un document partiel. Une erreur d'ecriture
//! rend l'instance inutilisable : apres un rename suivi d'un fsync en echec,
//! seul un rechargement peut determiner l'etat reel du disque.
//!
//! Les permissions Unix sont verifiees (0700/0600). Sur Windows, les ACL du
//! repertoire prive de donnees restent la frontiere d'acces. Aucun secret
//! n'est expose par Debug ni par les erreurs de deserialisation.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use super::identite::Identite;
use super::psk::{CategoriePsk, PskPair};
use serde::{Deserialize, Serialize};

const FICHIER: &str = "pairing.json";
const VERROU: &str = "pairing.lock";
const MAX_DOCUMENT: u64 = 8 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum MethodeAppairage {
    #[serde(rename = "pairing_psk")]
    Psk,
    #[serde(rename = "static_pairing_code")]
    CodeStatique,
    #[serde(rename = "dynamic_pairing_code")]
    CodeDynamique,
}

#[derive(Debug, thiserror::Error)]
pub enum ErreurMagasin {
    #[error("magasin Sendspin occupe par un autre processus")]
    Occupe,
    #[error("magasin Sendspin indisponible apres une erreur d'ecriture : recharger")]
    Indisponible,
    #[error("magasin Sendspin invalide : {0}")]
    Invalide(&'static str),
    #[error("stockage Sendspin : {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Enregistrement {
    psk: [u8; 32],
    methodes: BTreeSet<MethodeAppairage>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Document {
    version: u32,
    cle_privee: [u8; 32],
    pairs: BTreeMap<String, Enregistrement>,
}

impl Document {
    fn verifier(&self) -> Result<(), ErreurMagasin> {
        if self.version != 1 {
            return Err(ErreurMagasin::Invalide("version de stockage inconnue"));
        }
        for (id, record) in &self.pairs {
            if record.methodes.is_empty() {
                return Err(ErreurMagasin::Invalide("appairage sans methode"));
            }
            PskPair::pour_pair(id, record.psk, CategoriePsk::LongueDuree)
                .map_err(|_| ErreurMagasin::Invalide("identite ou PSK d'un pair invalide"))?;
        }
        Ok(())
    }
}

/// Vue publique du magasin, sans materiel secret.
#[derive(Debug, Serialize)]
pub struct PairAppaire {
    pub client_id: String,
    #[serde(rename = "pair_methods")]
    pub methodes: BTreeSet<MethodeAppairage>,
}

pub struct MagasinAppairage {
    dossier: PathBuf,
    _verrou: File,
    identite: Identite,
    document: Document,
    en_panne: bool,
}

impl std::fmt::Debug for MagasinAppairage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MagasinAppairage")
            .field("server_id", &self.identite.id())
            .field("nombre_pairs", &self.document.pairs.len())
            .field("en_panne", &self.en_panne)
            .finish_non_exhaustive()
    }
}

impl MagasinAppairage {
    /// Ouvrir un dossier dedie. Son parent doit etre controle par l'operateur.
    /// Aucun chemin ni repertoire global n'est choisi dans le coeur.
    pub fn ouvrir(dossier: &Path) -> Result<Self, ErreurMagasin> {
        let mut creation = fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            creation.mode(0o700);
        }
        match creation.create(dossier) {
            Ok(()) => (),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => (),
            Err(e) => return Err(e.into()),
        }
        verifier_chemin(dossier, true)?;
        verifier_fichier_si_present(&dossier.join(VERROU))?;
        let mut options = options_privees();
        let mut verrou = options
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(dossier.join(VERROU))?;
        verifier_fichier(&verrou)?;
        verrou.try_lock().map_err(|e| match e {
            fs::TryLockError::WouldBlock => ErreurMagasin::Occupe,
            fs::TryLockError::Error(e) => ErreurMagasin::Io(e),
        })?;

        let chemin = dossier.join(FICHIER);
        verifier_fichier_si_present(&chemin)?;
        let document = match options_privees().read(true).open(&chemin) {
            Ok(f) => {
                verifier_fichier(&f)?;
                let mut contenu = Vec::new();
                f.take(MAX_DOCUMENT + 1).read_to_end(&mut contenu)?;
                if contenu.len() as u64 > MAX_DOCUMENT {
                    return Err(ErreurMagasin::Invalide("document trop volumineux"));
                }
                serde_json::from_slice::<Document>(&contenu)
                    .map_err(|_| ErreurMagasin::Invalide("document illisible"))?
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                if verrou.metadata()?.len() != 0 {
                    return Err(ErreurMagasin::Invalide(
                        "document perdu apres initialisation",
                    ));
                }
                let identite = Identite::generer();
                let document = Document {
                    version: 1,
                    cle_privee: *identite.prive(),
                    pairs: BTreeMap::new(),
                };
                ecrire(dossier, &document)?;
                document
            }
            Err(e) => return Err(e.into()),
        };
        document.verifier()?;
        // Le verrou contient aussi un marqueur durable d'initialisation.
        // Un JSON manquant ensuite n'est jamais interprete comme un premier boot.
        if verrou.metadata()?.len() == 0 {
            verrou.write_all(b"1")?;
            verrou.sync_all()?;
            synchroniser_dossier(dossier)?;
        }
        // Le fsync du dossier rend ses fichiers durables ; celui du parent
        // rend durable l'entree du dossier lui-meme, y compris au premier boot.
        synchroniser_dossier(
            dossier
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or(Path::new(".")),
        )?;
        Ok(Self {
            dossier: dossier.to_owned(),
            _verrou: verrou,
            identite: Identite::depuis_prive(document.cle_privee),
            document,
            en_panne: false,
        })
    }

    pub fn identite(&self) -> &Identite {
        &self.identite
    }

    pub fn lister(&self) -> Result<Vec<PairAppaire>, ErreurMagasin> {
        self.verifier_disponibilite()?;
        Ok(self
            .document
            .pairs
            .iter()
            .map(|(id, record)| PairAppaire {
                client_id: id.clone(),
                methodes: record.methodes.clone(),
            })
            .collect())
    }

    pub fn cle_du_pair(&self, client_id: &str) -> Result<Option<PskPair>, ErreurMagasin> {
        self.verifier_disponibilite()?;
        self.document
            .pairs
            .get(client_id)
            .map(|r| {
                PskPair::pour_pair(client_id, r.psk, CategoriePsk::LongueDuree)
                    .map_err(|_| ErreurMagasin::Invalide("enregistrement incoherent"))
            })
            .transpose()
    }

    /// Appeler seulement apres verification mutuelle complete. Le succes doit
    /// preceder server/pair-finalize : il signifie que le disque a ete mis a jour.
    pub fn conserver(
        &mut self,
        client_id: &str,
        psk: &PskPair,
        methode: MethodeAppairage,
    ) -> Result<(), ErreurMagasin> {
        self.verifier_disponibilite()?;
        psk.verifier_pair(client_id)
            .map_err(|_| ErreurMagasin::Invalide("PSK liee a un autre pair"))?;
        if psk.categorie() != CategoriePsk::LongueDuree {
            return Err(ErreurMagasin::Invalide(
                "seule une PSK longue duree se conserve",
            ));
        }
        let mut suivant = self.document.clone();
        let mut methodes = BTreeSet::from([methode]);
        // Une nouvelle cle remplace le record. Une verification supplementaire
        // de la meme cle enrichit ses methodes sans perdre l'historique.
        if let Some(ancien) = suivant.pairs.get(client_id)
            && ancien.psk == *psk.secret()
        {
            methodes.extend(&ancien.methodes);
        }
        suivant.pairs.insert(
            client_id.to_owned(),
            Enregistrement {
                psk: *psk.secret(),
                methodes,
            },
        );
        self.publier(suivant)
    }

    pub fn retirer(&mut self, client_id: &str) -> Result<bool, ErreurMagasin> {
        self.verifier_disponibilite()?;
        let mut suivant = self.document.clone();
        if suivant.pairs.remove(client_id).is_none() {
            return Ok(false);
        }
        self.publier(suivant)?;
        Ok(true)
    }

    fn verifier_disponibilite(&self) -> Result<(), ErreurMagasin> {
        if self.en_panne {
            Err(ErreurMagasin::Indisponible)
        } else {
            Ok(())
        }
    }

    fn publier(&mut self, suivant: Document) -> Result<(), ErreurMagasin> {
        if let Err(e) = ecrire(&self.dossier, &suivant) {
            self.en_panne = true;
            return Err(e);
        }
        self.document = suivant;
        Ok(())
    }
}

fn options_privees() -> OpenOptions {
    let mut options = OpenOptions::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    options
}

fn verifier_chemin(chemin: &Path, dossier: bool) -> Result<(), ErreurMagasin> {
    let meta = fs::symlink_metadata(chemin)?;
    if (dossier && !meta.is_dir()) || (!dossier && !meta.is_file()) {
        return Err(ErreurMagasin::Invalide(
            "chemin non regulier ou lien symbolique",
        ));
    }
    verifier_permissions(&meta)
}

fn verifier_fichier_si_present(chemin: &Path) -> Result<(), ErreurMagasin> {
    match fs::symlink_metadata(chemin) {
        Ok(_) => verifier_chemin(chemin, false),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

fn verifier_fichier(f: &File) -> Result<(), ErreurMagasin> {
    let meta = f.metadata()?;
    if !meta.is_file() {
        return Err(ErreurMagasin::Invalide("fichier non regulier"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if meta.nlink() != 1 {
            return Err(ErreurMagasin::Invalide("fichier partage par lien physique"));
        }
    }
    verifier_permissions(&meta)
}

fn verifier_permissions(meta: &fs::Metadata) -> Result<(), ErreurMagasin> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o077 != 0 {
            return Err(ErreurMagasin::Invalide(
                "permissions ouvertes a d'autres comptes",
            ));
        }
    }
    #[cfg(not(unix))]
    let _ = meta;
    Ok(())
}

fn synchroniser_dossier(dossier: &Path) -> Result<(), ErreurMagasin> {
    #[cfg(unix)]
    File::open(dossier)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = dossier;
    Ok(())
}

fn ecrire(dossier: &Path, document: &Document) -> Result<(), ErreurMagasin> {
    document.verifier()?;
    let contenu = serde_json::to_vec(document)
        .map_err(|_| ErreurMagasin::Invalide("serialisation impossible"))?;
    if contenu.len() as u64 > MAX_DOCUMENT {
        return Err(ErreurMagasin::Invalide("capacite du magasin depassee"));
    }
    verifier_chemin(dossier, true)?;
    let destination = dossier.join(FICHIER);
    match fs::symlink_metadata(&destination) {
        Ok(_) => verifier_chemin(&destination, false)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
        Err(e) => return Err(e.into()),
    }
    let temporaire = dossier.join(format!(".pairing-{}.tmp", uuid::Uuid::new_v4()));
    let resultat = (|| {
        let mut f = options_privees()
            .write(true)
            .create_new(true)
            .open(&temporaire)?;
        f.write_all(&contenu)?;
        f.sync_all()?;
        drop(f);
        fs::rename(&temporaire, &destination)?;
        synchroniser_dossier(dossier)
    })();
    // Un temporaire incomplet n'est jamais utilise comme repli a la lecture.
    let _ = fs::remove_file(&temporaire);
    resultat
}
