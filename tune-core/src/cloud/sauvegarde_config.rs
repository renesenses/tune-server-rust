//! Sauvegarde cloud des personnalisations, automatique et tournante (#5654,
//! tune-web-client#902).
//!
//! Réservée au Premium, toutes ses routes comprises (seul l'état se lit sans,
//! pour que l'écran dise « Premium requis »).
//!
//! Le serveur range dans le compte mozaiklabs relié au plus
//! [`MAX_INSTANTANES`] instantanés CHIFFRÉS par machine, et le compte garde
//! au plus [`MAX_MACHINES`] machines (le site élague les plus anciennes), de
//! ce qui fait « son » Tune :
//! réglages, zones et leurs réglages, profils et leurs préférences
//! d'interface (`ui_preferences`, thème…), préréglages d'égaliseur et profils
//! de pièce, favoris, playlists, radios. Sur un appareil neuf relié au même
//! compte, la liste se relit et un instantané se restaure, en fusionnant ou
//! en remplaçant.
//!
//! # Ce qui n'y entre JAMAIS
//!
//! - la base de la bibliothèque (pistes, albums, artistes) et ce qui la
//!   pilote (dossiers de musique, réglages de scan) ;
//! - les jetons et secrets des services, les mots de passe, les clés : tout
//!   nom que [`crate::secrets::est_secret`] reconnaît, à l'export ET à la
//!   restauration, puis une seconde passe sur l'objet entier ;
//! - l'identité de la machine et sa liaison au compte (`server_id`, jetons
//!   du compte, licence, propriétaire), et l'état de la sauvegarde elle-même ;
//! - le hash de mot de passe des profils, et leur droit d'administration :
//!   un profil restauré revient SANS mot de passe
//!   ([`Rapport::profiles_without_password`]), et l'écran l'annonce.
//!
//! # Chiffrement
//!
//! Chaque instantané est compressé puis scellé en XChaCha20-Poly1305 sous une
//! clé de données (DEK) aléatoire de 256 bits, enveloppée deux fois par
//! Argon2id : sous la phrase de passe choisie à l'activation, et sous une clé
//! de secours montrée UNE fois ([`crate::secret_envelope`]). Le serveur garde
//! la DEK en local (réglage secret, jamais exporté) pour sceller sans que
//! personne ne tape rien ; les deux emplacements voyagent avec chaque
//! instantané, de sorte qu'une machine neuve rouvre la DEK avec l'un des deux
//! secrets. Le site ne reçoit ni la phrase de passe, ni la clé de secours, ni
//! la DEK : il stocke un texte opaque.
//!
//! # Modèle de menace
//!
//! - **Le site (ou qui lit sa base)** voit : le compte, le `server_id`, le
//!   nom de la machine s'il est posé, l'empreinte courte de la clé
//!   (`key_id`, un condensé qui ne permet pas de la retrouver), la taille et
//!   la date de chaque instantané. Il ne peut pas lire le contenu ni le
//!   modifier sans que l'ouverture échoue (AEAD). Il PEUT le supprimer, le
//!   refuser, ou servir un instantané plus ancien du même compte à la place
//!   du plus récent (retour en arrière) : la date affichée après
//!   déchiffrement est celle écrite DANS l'instantané, que le site ne peut
//!   pas falsifier.
//! - **Une attaque hors ligne sur un instantané volé** vise la phrase de
//!   passe (Argon2id, sel par emplacement ; 10 caractères minimum) ou la clé
//!   de secours (160 bits aléatoires, hors de portée).
//! - **Qui détient la machine** détient la DEK locale, comme il détient déjà
//!   la configuration en clair : le chiffrement protège la copie hors
//!   machine, pas la machine.
//! - **Qui vole le jeton du compte** peut lister, télécharger et supprimer
//!   les instantanés, pas les lire. Le dépôt exige en plus le jeton de
//!   liaison de la machine.
//! - **Perte des deux secrets** : les instantanés deviennent illisibles sur
//!   une autre machine, sans recours — c'est le prix du chiffrement de bout
//!   en bout. La machine d'origine, qui garde la DEK, continue de les ouvrir.

use std::io::{Read, Write};
use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use tracing::{info, warn};

use crate::db::backend::{DbBackend, ToSqlValue};
use crate::db::favorites_reconcile::FavoritesReconciler;
use crate::db::profile_repo::ProfileRepo;
use crate::db::settings_repo::SettingsRepo;
use crate::secret_envelope::{Envelope, KeySlot, RecoveryKey, hex_decode, hex_encode};
use crate::secrets::{est_secret, retirer_les_secrets};

// ── Constantes ──────────────────────────────────────────────────────

/// `format` du contenu (clair, après déchiffrement).
pub const FORMAT: &str = "tune-personnalisations";
/// Version du contenu écrite par ce serveur.
pub const FORMAT_VERSION: u64 = 1;
/// `format` de l'enveloppe chiffrée déposée au site.
pub const FORMAT_BLOB: &str = "tune-config-cloud";
/// Version de l'enveloppe.
pub const FORMAT_BLOB_VERSION: u64 = 1;

/// Instantanés gardés par MACHINE (la rotation est faite par le site).
pub const MAX_INSTANTANES: usize = 3;
/// Machines qui sauvegardent dans un même compte ; au-delà, le site élague
/// la machine dont la dernière sauvegarde est la plus ancienne.
pub const MAX_MACHINES: usize = 5;
/// Cadence de la passe de fond.
pub const CADENCE_MINUTES: i64 = 5;
/// Délai d'attente : un changement n'est envoyé qu'une fois la configuration
/// stable depuis ce temps, pour qu'une séance de réglages ne produise qu'un
/// instantané.
pub const DELAI_D_ATTENTE_MINUTES: i64 = 10;
/// Au-delà, un changement qui ne se stabilise pas part quand même.
pub const ATTENTE_MAXIMALE_MINUTES: i64 = 60;
/// Un instantané par jour au plus : dans la journée, le plus récent de CE
/// serveur est remplacé, si bien que les trois instantanés couvrent trois
/// journées distinctes plutôt que trois minutes d'une même séance.
pub const RENOUVELLEMENT_HEURES: i64 = 24;
/// Après un échec, la passe automatique attend avant de réessayer.
pub const REPRISE_APRES_ECHEC_MINUTES: i64 = 30;
/// Longueur minimale de la phrase de passe.
pub const PHRASE_MIN: usize = 10;
/// Taille maximale d'un contenu décompressé (garde contre un blob piégé).
const DECOMPRESSE_MAX: u64 = 64 * 1024 * 1024;

/// Racine du site par défaut (réglage `mozaik_base_url` sinon).
const SITE_PAR_DEFAUT: &str = "https://mozaiklabs.fr";
/// Chemin de l'API du site.
const CHEMIN_API: &str = "/api/v1/config-backups";

/// La DEK en hexadécimal. Le nom porte `secret` : [`est_secret`] l'écarte de
/// TOUT export, de `/system/config` et de cette sauvegarde même.
pub const CLE_SECRETE: &str = "sauvegarde_cloud_cle_secrete";
/// Les deux emplacements de clé et le `key_id` (sans secret au repos).
pub const CLE_TROUSSEAU: &str = "sauvegarde_cloud_trousseau";
pub const CLE_ACTIVE: &str = "sauvegarde_cloud_active";
pub const CLE_DERNIERE: &str = "sauvegarde_cloud_derniere";
pub const CLE_DERNIERE_EMPREINTE: &str = "sauvegarde_cloud_derniere_empreinte";
pub const CLE_DERNIER_ID: &str = "sauvegarde_cloud_dernier_id";
pub const CLE_DERNIERE_TENTATIVE: &str = "sauvegarde_cloud_derniere_tentative";
pub const CLE_DERNIERE_ERREUR: &str = "sauvegarde_cloud_derniere_erreur";
pub const CLE_ATTENTE_EMPREINTE: &str = "sauvegarde_cloud_attente_empreinte";
pub const CLE_ATTENTE_DEPUIS: &str = "sauvegarde_cloud_attente_depuis";
pub const CLE_ATTENTE_PREMIERE: &str = "sauvegarde_cloud_attente_premiere";

/// Préférences rangées par profil sous `cle:{pid}`. Elles voyagent DANS leur
/// profil et sont réécrites sous le numéro du profil d'arrivée.
const PREFS_PAR_PROFIL: &[&str] = &["ui_preferences", "theme", "metadata_visible_fields"];

/// Réglages exacts qui ne voyagent pas : identité de la machine et
/// bibliothèque.
const EXCLUS_EXACTS: &[&str] = &["server_id", "owner_profile_id", "music_dirs"];

/// Préfixes de réglages qui ne voyagent pas.
///
/// - compte, liaison, licence, propriétaire : propres à CETTE machine ;
/// - `config_backup_` : l'enveloppe des jetons de la sauvegarde Premium
///   manuelle — chiffrée, mais ce sont des jetons ;
/// - `sauvegarde_cloud_` : l'état de cette sauvegarde-ci ;
/// - `library_`, `scan_`, `last_` : la bibliothèque et des états ;
/// - le reste : appairages, sauvegardes de base (chemins locaux), matériel.
const EXCLUS_PREFIXES: &[&str] = &[
    "mozaik_",
    "cloud_",
    "config_backup_",
    "sauvegarde_cloud_",
    "license_",
    "library_",
    "scan_",
    "last_",
    "onboarding",
    "db_backup",
    "hardware_",
    "airplay2_pairing",
    "credentials_",
    "telemetry_",
];

// ── Ce qui part ─────────────────────────────────────────────────────

/// Clé de profil `cle:{pid}` reconnue : `(cle, pid)`.
fn pref_de_profil(cle: &str) -> Option<(&'static str, i64)> {
    let (nom, pid) = cle.rsplit_once(':')?;
    let pid: i64 = pid.parse().ok()?;
    PREFS_PAR_PROFIL
        .iter()
        .find(|p| **p == nom)
        .map(|p| (*p, pid))
}

/// Ce réglage reste-t-il sur la machine ? Vrai pour tout secret, toute clé
/// d'identité ou de bibliothèque, et toute clé suffixée d'un numéro local
/// (`crossfade_enabled:7`) qui ne serait pas une préférence de profil : ce
/// numéro ne désigne rien ailleurs.
pub fn reste_sur_la_machine(cle: &str) -> bool {
    if est_secret(cle) {
        return true;
    }
    let min = cle.to_ascii_lowercase();
    if EXCLUS_EXACTS.contains(&min.as_str()) {
        return true;
    }
    if EXCLUS_PREFIXES.iter().any(|p| min.starts_with(p)) {
        return true;
    }
    if let Some((_, suffixe)) = cle.rsplit_once(':')
        && !suffixe.is_empty()
        && suffixe.bytes().all(|b| b.is_ascii_digit())
    {
        return pref_de_profil(cle).is_none();
    }
    false
}

/// Les favoris locaux, sous une identité lisible : un identifiant de
/// bibliothèque ne désigne rien sur une autre machine.
fn favoris_exportes(backend: &Arc<dyn DbBackend>) -> Result<Vec<Value>, String> {
    let lignes = backend.query_many(
        "SELECT profile_id, item_type, item_id FROM favorites \
         WHERE item_type IN ('track', 'album', 'artist', 'playlist') ORDER BY id",
        &[],
    )?;
    let rec = FavoritesReconciler::with_backend(backend.clone());
    let mut sortie = Vec::with_capacity(lignes.len());
    for l in lignes {
        let profil = l.first().and_then(|v| v.as_i64()).unwrap_or(1);
        let Some(genre) = l.get(1).and_then(|v| v.as_string()) else {
            continue;
        };
        let Some(id) = l.get(2).and_then(|v| v.as_i64()) else {
            continue;
        };
        if let Some((nom, artiste, chemin)) = rec.identite_vivante(&genre, id)? {
            sortie.push(json!({
                "profile_id": profil,
                "item_type": genre,
                "name": nom,
                "artist": artiste,
                "path": chemin,
            }));
        }
    }
    Ok(sortie)
}

/// Construit le contenu EN CLAIR d'un instantané (avant chiffrement).
///
/// `created_at` n'en fait pas partie : voir [`empreinte`] ; il est ajouté par
/// [`horodater`].
pub fn construire(backend: &Arc<dyn DbBackend>) -> Result<Value, String> {
    let base = crate::config_export::exporter(backend, false)?;
    let reglages_bruts = base
        .get("settings")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();

    let profils = ProfileRepo::with_backend(backend.clone()).list()?;
    let mut prefs: std::collections::BTreeMap<i64, Map<String, Value>> = Default::default();
    let mut reglages = Map::new();
    for (cle, valeur) in reglages_bruts {
        if let Some((nom, pid)) = pref_de_profil(&cle) {
            prefs
                .entry(pid)
                .or_default()
                .insert(nom.to_string(), valeur);
            continue;
        }
        if reste_sur_la_machine(&cle) {
            continue;
        }
        reglages.insert(cle, valeur);
    }

    let profils: Vec<Value> = profils
        .into_iter()
        .filter_map(|p| {
            let id = p.id?;
            Some(json!({
                "id": id,
                "username": p.name,
                "display_name": p.display_name,
                "prefs": Value::Object(prefs.remove(&id).unwrap_or_default()),
            }))
        })
        .collect();

    let mut contenu = Map::new();
    contenu.insert("format".into(), json!(FORMAT));
    contenu.insert("format_version".into(), json!(FORMAT_VERSION));
    contenu.insert("settings".into(), Value::Object(reglages));
    contenu.insert(
        "zones".into(),
        base.get("zones").cloned().unwrap_or_else(|| json!([])),
    );
    contenu.insert("profiles".into(), Value::Array(profils));
    contenu.insert(
        "playlists".into(),
        Value::Array(crate::config_backup::export_playlists(backend)?),
    );
    contenu.insert("favorites".into(), Value::Array(favoris_exportes(backend)?));
    contenu.insert(
        "radio_stations".into(),
        Value::Array(crate::config_backup::export_radios(backend)?),
    );
    // Seconde passe, sur TOUT l'objet : un secret imbriqué dans un réglage de
    // zone, une préférence ou une ligne exotique ne part pas davantage.
    retirer_les_secrets(&mut contenu);
    Ok(Value::Object(contenu))
}

/// Empreinte du contenu, date exclue : deux passes sur une configuration
/// inchangée donnent la même, et la seconde n'envoie rien.
pub fn empreinte(contenu: &Value) -> String {
    let mut c = contenu.clone();
    if let Some(o) = c.as_object_mut() {
        o.remove("created_at");
        o.remove("server_version");
    }
    let octets = serde_json::to_vec(&c).unwrap_or_default();
    format!("{:x}", Sha256::digest(&octets))
}

/// Ajoute la date et la version au contenu.
pub fn horodater(contenu: &mut Value, maintenant: DateTime<Utc>) {
    if let Some(o) = contenu.as_object_mut() {
        o.insert(
            "created_at".into(),
            json!(maintenant.format("%Y-%m-%dT%H:%M:%SZ").to_string()),
        );
        o.insert("server_version".into(), json!(crate::version()));
    }
}

// ── Clé ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Trousseau {
    key_id: String,
    passphrase_slot: KeySlot,
    recovery_slot: KeySlot,
}

/// La clé de CE serveur.
pub struct CleLocale {
    dek: [u8; 32],
    pub key_id: String,
    passphrase_slot: KeySlot,
    recovery_slot: KeySlot,
}

/// Empreinte courte et publique d'une DEK : distingue deux clés sans rien
/// révéler de l'une ni de l'autre.
fn key_id_de(dek: &[u8; 32]) -> String {
    let mut h = Sha256::new();
    h.update(b"tune-config-cloud/key-id/v1");
    h.update(dek);
    hex_encode(&h.finalize()[..8])
}

/// La clé de ce serveur, si la sauvegarde a été activée (ou adoptée).
pub fn cle_locale(settings: &SettingsRepo) -> Result<Option<CleLocale>, String> {
    let (Some(hex), Some(brut)) = (settings.get(CLE_SECRETE)?, settings.get(CLE_TROUSSEAU)?) else {
        return Ok(None);
    };
    if hex.trim().is_empty() || brut.trim().is_empty() {
        return Ok(None);
    }
    let dek: [u8; 32] = hex_decode(hex.trim())?
        .try_into()
        .map_err(|_| "stored backup key has the wrong length".to_string())?;
    let t: Trousseau =
        serde_json::from_str(&brut).map_err(|e| format!("stored backup keyring: {e}"))?;
    if key_id_de(&dek) != t.key_id {
        return Err("stored backup key does not match its keyring".into());
    }
    Ok(Some(CleLocale {
        dek,
        key_id: t.key_id,
        passphrase_slot: t.passphrase_slot,
        recovery_slot: t.recovery_slot,
    }))
}

fn ranger_la_cle(
    settings: &SettingsRepo,
    dek: &[u8; 32],
    passphrase_slot: &KeySlot,
    recovery_slot: &KeySlot,
) -> Result<String, String> {
    let key_id = key_id_de(dek);
    let t = Trousseau {
        key_id: key_id.clone(),
        passphrase_slot: passphrase_slot.clone(),
        recovery_slot: recovery_slot.clone(),
    };
    settings.set(
        CLE_TROUSSEAU,
        &serde_json::to_string(&t).map_err(|e| e.to_string())?,
    )?;
    settings.set(CLE_SECRETE, &hex_encode(dek))?;
    Ok(key_id)
}

/// Crée la clé de ce serveur. Refuse s'il en a déjà une : la remplacer en
/// silence rendrait illisibles, sur une autre machine, les instantanés déjà
/// déposés sous l'ancienne.
pub fn creer_la_cle(
    settings: &SettingsRepo,
    passphrase: &str,
) -> Result<(String, RecoveryKey), String> {
    if passphrase.chars().count() < PHRASE_MIN {
        return Err(format!(
            "passphrase must be at least {PHRASE_MIN} characters"
        ));
    }
    if cle_locale(settings)?.is_some() {
        return Err("a backup key is already configured".into());
    }
    let (ps, rs, secours, dek) = Envelope::nouvelle_cle(passphrase)?;
    let key_id = ranger_la_cle(settings, &dek, &ps, &rs)?;
    info!(%key_id, "sauvegarde_cloud_cle_creee");
    Ok((key_id, secours))
}

// ── Enveloppe chiffrée ──────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Blob {
    format: String,
    format_version: u64,
    key_id: String,
    envelope: Envelope,
}

/// Compresse et scelle un contenu. Rend le texte opaque déposé au site.
pub fn chiffrer(cle: &CleLocale, contenu: &Value) -> Result<String, String> {
    let clair = serde_json::to_vec(contenu).map_err(|e| e.to_string())?;
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gz.write_all(&clair).map_err(|e| e.to_string())?;
    let compresse = gz.finish().map_err(|e| e.to_string())?;
    let envelope = Envelope::sceller_avec(
        &cle.dek,
        &cle.passphrase_slot,
        &cle.recovery_slot,
        &compresse,
    )?;
    serde_json::to_string(&Blob {
        format: FORMAT_BLOB.into(),
        format_version: FORMAT_BLOB_VERSION,
        key_id: cle.key_id.clone(),
        envelope,
    })
    .map_err(|e| e.to_string())
}

/// Pourquoi un instantané ne s'ouvre pas.
#[derive(Debug, PartialEq, Eq)]
pub enum ErreurOuverture {
    /// La clé de ce serveur n'est pas celle de l'instantané : il faut la
    /// phrase de passe ou la clé de secours.
    SecretRequis,
    /// Le secret fourni n'ouvre pas l'instantané.
    MauvaisSecret,
    /// Instantané illisible (format, corruption, altération).
    Illisible(String),
}

/// Ce qu'une ouverture par secret apprend : la DEK et ses emplacements, pour
/// que ce serveur puisse l'adopter.
pub struct CleRetrouvee {
    dek: [u8; 32],
    passphrase_slot: KeySlot,
    recovery_slot: KeySlot,
}

/// Ouvre un instantané : par la clé locale si c'est la sienne, sinon par le
/// secret (phrase de passe ou clé de secours).
pub fn dechiffrer(
    blob: &str,
    locale: Option<&CleLocale>,
    secret: Option<&str>,
) -> Result<(Value, Option<CleRetrouvee>), ErreurOuverture> {
    let b: Blob = serde_json::from_str(blob)
        .map_err(|e| ErreurOuverture::Illisible(format!("not a backup: {e}")))?;
    if b.format != FORMAT_BLOB || b.format_version > FORMAT_BLOB_VERSION {
        return Err(ErreurOuverture::Illisible(format!(
            "unsupported backup format {} v{}",
            b.format, b.format_version
        )));
    }
    let (compresse, retrouvee) = match locale.filter(|c| c.key_id == b.key_id) {
        Some(c) => (
            b.envelope
                .ouvrir_avec(&c.dek)
                .map_err(ErreurOuverture::Illisible)?,
            None,
        ),
        None => {
            let secret = secret
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .ok_or(ErreurOuverture::SecretRequis)?;
            let dek = b
                .envelope
                .cle_de_donnees(secret)
                .map_err(|_| ErreurOuverture::MauvaisSecret)?;
            let compresse = b
                .envelope
                .ouvrir_avec(&dek)
                .map_err(ErreurOuverture::Illisible)?;
            (
                compresse,
                Some(CleRetrouvee {
                    dek,
                    passphrase_slot: b.envelope.passphrase_slot.clone(),
                    recovery_slot: b.envelope.recovery_slot.clone(),
                }),
            )
        }
    };
    let mut clair = Vec::new();
    flate2::read::GzDecoder::new(&compresse[..])
        .take(DECOMPRESSE_MAX)
        .read_to_end(&mut clair)
        .map_err(|e| ErreurOuverture::Illisible(format!("decompress: {e}")))?;
    let contenu: Value = serde_json::from_slice(&clair)
        .map_err(|e| ErreurOuverture::Illisible(format!("content: {e}")))?;
    Ok((contenu, retrouvee))
}

/// Adopte la clé d'un instantané ouvert par secret, si ce serveur n'en a
/// pas : ses sauvegardes suivantes resteront lisibles avec la phrase de passe
/// et la clé de secours que l'utilisateur détient déjà. Rend `true` si
/// adoptée.
pub fn adopter(settings: &SettingsRepo, cle: &CleRetrouvee) -> Result<bool, String> {
    if cle_locale(settings)?.is_some() {
        return Ok(false);
    }
    let key_id = ranger_la_cle(settings, &cle.dek, &cle.passphrase_slot, &cle.recovery_slot)?;
    info!(%key_id, "sauvegarde_cloud_cle_adoptee");
    Ok(true)
}

// ── Restauration ────────────────────────────────────────────────────

/// Fusionner : l'existant l'emporte, seul ce qui manque est ajouté.
/// Remplacer : l'instantané l'emporte en cas de conflit. Dans les deux cas,
/// rien n'est supprimé.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Merge,
    Replace,
}

/// Ce qu'une restauration a fait.
#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct Rapport {
    pub settings_written: usize,
    pub zones_created: usize,
    pub zones_updated: usize,
    pub profiles_created: usize,
    /// Profils créés par la restauration, donc SANS mot de passe : le hash
    /// ne voyage jamais. L'écran le dit et invite à en poser un.
    pub profiles_without_password: Vec<String>,
    pub playlists_restored: usize,
    pub playlists_replaced: usize,
    pub favorites_restored: usize,
    pub radios_restored: usize,
    pub warnings: Vec<String>,
}

fn texte(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        autre => autre.to_string(),
    }
}

/// Restaure un contenu déchiffré. N'écrit JAMAIS dans les tables de la
/// bibliothèque (pistes, albums, artistes) ni dans les réglages qui la
/// pilotent : [`reste_sur_la_machine`] est réappliqué à l'arrivée, un
/// instantané piégé ne passe pas davantage qu'un export.
pub fn restaurer(
    backend: &Arc<dyn DbBackend>,
    contenu: &Value,
    mode: Mode,
) -> Result<Rapport, String> {
    let o = contenu
        .as_object()
        .ok_or_else(|| "backup content is not an object".to_string())?;
    if o.get("format").and_then(Value::as_str) != Some(FORMAT) {
        return Err("backup content has an unknown format".into());
    }
    let version = o.get("format_version").and_then(Value::as_u64).unwrap_or(0);
    if version == 0 || version > FORMAT_VERSION {
        return Err(format!(
            "unsupported content version {version} (this server reads up to {FORMAT_VERSION})"
        ));
    }
    let settings = SettingsRepo::with_backend(backend.clone());
    let mut rapport = Rapport::default();

    // ── Profils : rapprochés par nom d'utilisateur ──
    let repo = ProfileRepo::with_backend(backend.clone());
    let mut vers_profil: std::collections::HashMap<i64, i64> = Default::default();
    let existants = repo.list()?;
    for p in o
        .get("profiles")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
    {
        let (Some(source), Some(nom)) = (
            p.get("id").and_then(Value::as_i64),
            p.get("username")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|n| !n.is_empty()),
        ) else {
            continue;
        };
        let affiche = p.get("display_name").and_then(Value::as_str);
        let cible = match existants.iter().find(|e| e.name == nom).and_then(|e| e.id) {
            Some(id) => {
                if mode == Mode::Replace
                    && let Some(a) = affiche
                {
                    // Pas `ProfileRepo::update`, qui réécrit aussi le nom
                    // d'utilisateur avec le nom affiché.
                    backend.execute(
                        "UPDATE profiles SET display_name = ? WHERE id = ?",
                        &[&a.to_string() as &dyn ToSqlValue, &id as &dyn ToSqlValue],
                    )?;
                }
                id
            }
            None => {
                // Ni mot de passe ni droit d'administration : ils ne
                // voyagent pas (voir l'en-tête).
                let id = repo.create(nom, affiche, None)?;
                rapport.profiles_created += 1;
                rapport.profiles_without_password.push(nom.to_string());
                id
            }
        };
        vers_profil.insert(source, cible);
        if let Some(prefs) = p.get("prefs").and_then(Value::as_object) {
            for (cle, valeur) in prefs {
                if !PREFS_PAR_PROFIL.contains(&cle.as_str()) {
                    continue;
                }
                let k = format!("{cle}:{cible}");
                if mode == Mode::Merge && settings.get(&k)?.is_some() {
                    continue;
                }
                settings.set(&k, &texte(valeur))?;
                rapport.settings_written += 1;
            }
        }
    }

    // ── Réglages et zones : le moteur de l'export gratuit ──
    let mut reglages = Map::new();
    for (cle, valeur) in o
        .get("settings")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default()
    {
        if reste_sur_la_machine(&cle) || pref_de_profil(&cle).is_some() {
            continue;
        }
        reglages.insert(cle, valeur);
    }
    let mut corps = Map::new();
    corps.insert(
        "format_version".into(),
        json!(crate::config_export::FORMAT_VERSION),
    );
    corps.insert("settings".into(), Value::Object(reglages));
    corps.insert(
        "zones".into(),
        o.get("zones").cloned().unwrap_or_else(|| json!([])),
    );
    let fichier = crate::config_export::lire(corps)?;
    let mut plan = crate::config_export::planifier(backend, &fichier)?;
    if mode == Mode::Merge {
        use crate::config_export::Statut;
        plan.reglages.retain(|r| r.statut == Statut::Ajoute);
        for z in plan.zones.iter_mut() {
            if z.statut == Statut::Modifie {
                z.statut = Statut::Inchange;
                z.changements.clear();
            }
        }
    }
    let bilan = crate::config_export::appliquer(backend, &plan)?;
    rapport.settings_written += bilan.reglages_ecrits;
    rapport.zones_created = bilan.zones_creees.len();
    rapport.zones_updated = bilan.zones_modifiees.len();
    rapport.warnings.extend(bilan.avertissements);

    // ── Playlists ──
    for pl in o
        .get("playlists")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
    {
        let Some(nom) = pl
            .get("name")
            .and_then(Value::as_str)
            .filter(|n| !n.trim().is_empty())
        else {
            continue;
        };
        let pistes = pl
            .get("tracks")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let existante = backend
            .query_one(
                "SELECT id FROM playlists WHERE name = ?",
                &[&nom.to_string() as &dyn ToSqlValue],
            )?
            .and_then(|r| r.first().and_then(|v| v.as_i64()));
        match (existante, mode) {
            (Some(_), Mode::Merge) => continue,
            (Some(id), Mode::Replace) => {
                // Même identifiant : les favoris et les alarmes qui la citent
                // continuent de la désigner.
                backend.execute(
                    "DELETE FROM playlist_tracks WHERE playlist_id = ?",
                    &[&id as &dyn ToSqlValue],
                )?;
                crate::config_backup::inserer_les_pistes(
                    backend,
                    id,
                    nom,
                    &pistes,
                    &mut rapport.warnings,
                )?;
                rapport.playlists_replaced += 1;
            }
            (None, _) => {
                let desc = pl
                    .get("description")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                let id = backend.execute_returning_id(
                    "INSERT INTO playlists (name, description) VALUES (?, ?)",
                    &[
                        &nom.to_string() as &dyn ToSqlValue,
                        &desc as &dyn ToSqlValue,
                    ],
                )?;
                crate::config_backup::inserer_les_pistes(
                    backend,
                    id,
                    nom,
                    &pistes,
                    &mut rapport.warnings,
                )?;
                rapport.playlists_restored += 1;
            }
        }
    }

    // ── Favoris : retrouvés par identité dans CETTE bibliothèque ──
    let rec = FavoritesReconciler::with_backend(backend.clone());
    let mut introuvables = 0usize;
    for f in o
        .get("favorites")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
    {
        let genre = f
            .get("item_type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if !matches!(genre, "track" | "album" | "artist" | "playlist") {
            continue;
        }
        let source = f.get("profile_id").and_then(Value::as_i64).unwrap_or(1);
        let profil = vers_profil.get(&source).copied().unwrap_or(1);
        let champ = |k: &str| {
            f.get(k)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        };
        let Some(id) = rec.retrouver(genre, &champ("name"), &champ("artist"), &champ("path"))?
        else {
            introuvables += 1;
            continue;
        };
        let poses = backend.execute(
            "INSERT INTO favorites (profile_id, item_type, item_id) VALUES (?, ?, ?) \
             ON CONFLICT (profile_id, item_type, item_id) DO NOTHING",
            &[
                &profil as &dyn ToSqlValue,
                &genre.to_string() as &dyn ToSqlValue,
                &id as &dyn ToSqlValue,
            ],
        )?;
        if poses > 0 {
            rec.snapshot_item(genre, id);
            rapport.favorites_restored += 1;
        }
    }
    if introuvables > 0 {
        rapport.warnings.push(format!(
            "{introuvables} favorite(s) not found in this library; skipped"
        ));
    }

    // ── Radios ──
    let radios = o
        .get("radio_stations")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    rapport.radios_restored =
        crate::config_backup::import_radios(backend, &radios, &mut rapport.warnings)?;
    if mode == Mode::Replace {
        for r in &radios {
            let (Some(nom), Some(url)) = (
                r.get("name").and_then(Value::as_str),
                r.get("url").and_then(Value::as_str),
            ) else {
                continue;
            };
            backend.execute(
                "UPDATE radio_stations SET is_favorite = ? WHERE name = ? AND url = ?",
                &[
                    &r.get("is_favorite").and_then(Value::as_i64).unwrap_or(0) as &dyn ToSqlValue,
                    &nom.to_string() as &dyn ToSqlValue,
                    &url.to_string() as &dyn ToSqlValue,
                ],
            )?;
        }
    }

    info!(?mode, ?rapport, "sauvegarde_cloud_restauree");
    Ok(rapport)
}

// ── Planification ───────────────────────────────────────────────────

/// L'état de la passe automatique, tel que rangé dans les réglages.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Etat {
    pub derniere_empreinte: Option<String>,
    pub derniere: Option<DateTime<Utc>>,
    pub dernier_id: Option<i64>,
    pub attente_empreinte: Option<String>,
    pub attente_depuis: Option<DateTime<Utc>>,
    pub attente_premiere: Option<DateTime<Utc>>,
}

/// Ce que la passe doit faire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Rien n'a changé depuis le dernier envoi.
    Rien,
    /// Un changement nouveau : noter son empreinte et attendre.
    Noter,
    /// Un changement en attente, pas encore stable assez longtemps.
    Attendre,
    /// Envoyer ; `remplace` = l'instantané du jour de CE serveur à remplacer.
    Envoyer { remplace: Option<i64> },
}

/// La décision de la passe automatique, sans effet de bord.
pub fn decider(etat: &Etat, empreinte: &str, maintenant: DateTime<Utc>) -> Decision {
    if etat.derniere_empreinte.as_deref() == Some(empreinte) {
        return Decision::Rien;
    }
    if etat.attente_empreinte.as_deref() != Some(empreinte) {
        // Un changement trop long à se stabiliser part quand même.
        if etat
            .attente_premiere
            .is_some_and(|p| maintenant - p >= Duration::minutes(ATTENTE_MAXIMALE_MINUTES))
        {
            return Decision::Envoyer {
                remplace: remplacable(etat, maintenant),
            };
        }
        return Decision::Noter;
    }
    match etat.attente_depuis {
        Some(d) if maintenant - d >= Duration::minutes(DELAI_D_ATTENTE_MINUTES) => {
            Decision::Envoyer {
                remplace: remplacable(etat, maintenant),
            }
        }
        _ => Decision::Attendre,
    }
}

/// L'instantané de CE serveur à remplacer : le dernier, s'il a moins d'un
/// jour.
fn remplacable(etat: &Etat, maintenant: DateTime<Utc>) -> Option<i64> {
    match (etat.derniere, etat.dernier_id) {
        (Some(d), Some(id)) if maintenant - d < Duration::hours(RENOUVELLEMENT_HEURES) => Some(id),
        _ => None,
    }
}

fn date(settings: &SettingsRepo, cle: &str) -> Option<DateTime<Utc>> {
    settings
        .get(cle)
        .ok()
        .flatten()
        .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
        .map(|d| d.with_timezone(&Utc))
}

fn iso(d: DateTime<Utc>) -> String {
    d.format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

fn lire_etat(settings: &SettingsRepo) -> Etat {
    let txt = |k: &str| settings.get(k).ok().flatten().filter(|s| !s.is_empty());
    Etat {
        derniere_empreinte: txt(CLE_DERNIERE_EMPREINTE),
        derniere: date(settings, CLE_DERNIERE),
        dernier_id: txt(CLE_DERNIER_ID).and_then(|s| s.parse().ok()),
        attente_empreinte: txt(CLE_ATTENTE_EMPREINTE),
        attente_depuis: date(settings, CLE_ATTENTE_DEPUIS),
        attente_premiere: date(settings, CLE_ATTENTE_PREMIERE),
    }
}

/// La sauvegarde automatique est-elle activée ?
pub fn active(settings: &SettingsRepo) -> bool {
    settings
        .get(CLE_ACTIVE)
        .ok()
        .flatten()
        .is_some_and(|v| v == "true" || v == "1")
}

pub fn activer(settings: &SettingsRepo, oui: bool) -> Result<(), String> {
    settings.set(CLE_ACTIVE, if oui { "true" } else { "false" })
}

/// L'état lu par l'écran.
pub fn statut(settings: &SettingsRepo) -> Value {
    let txt = |k: &str| settings.get(k).ok().flatten().filter(|s| !s.is_empty());
    json!({
        "enabled": active(settings),
        "key_configured": cle_locale(settings).ok().flatten().is_some(),
        "account_linked": compte(settings).is_ok(),
        "last_backup_at": txt(CLE_DERNIERE),
        "last_attempt_at": txt(CLE_DERNIERE_TENTATIVE),
        "last_error": txt(CLE_DERNIERE_ERREUR),
        "pending_since": txt(CLE_ATTENTE_PREMIERE),
        "debounce_minutes": DELAI_D_ATTENTE_MINUTES,
        "max_snapshots": MAX_INSTANTANES,
        "max_machines": MAX_MACHINES,
    })
}

// ── Site ────────────────────────────────────────────────────────────

/// Un instantané tel que le site le décrit.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Meta {
    pub id: i64,
    pub server_id: String,
    #[serde(default)]
    pub server_label: Option<String>,
    pub key_id: String,
    pub format_version: i64,
    pub size_bytes: i64,
    pub created_at: String,
}

/// Pourquoi un échange avec le site a échoué.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ErreurSite {
    /// Pas de session SSO, ou pas de `server_id`.
    NonRelie,
    /// Le site a répondu `statut` ; `code` = son `error` s'il en a un.
    Http {
        statut: u16,
        code: Option<String>,
    },
    Reseau(String),
    Illisible,
}

impl std::fmt::Display for ErreurSite {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ErreurSite::NonRelie => write!(f, "account_not_linked"),
            ErreurSite::Http { statut, code } => match code {
                Some(c) => write!(f, "HTTP {statut} {c}"),
                None => write!(f, "HTTP {statut}"),
            },
            ErreurSite::Reseau(e) => write!(f, "network: {e}"),
            ErreurSite::Illisible => write!(f, "unreadable response"),
        }
    }
}

/// Ce qu'il faut pour parler au site au nom du compte.
#[derive(Debug, Clone)]
pub struct Compte {
    base: String,
    acces: String,
    liaison: Option<String>,
    pub server_id: String,
}

/// Le compte relié, ou [`ErreurSite::NonRelie`].
pub fn compte(settings: &SettingsRepo) -> Result<Compte, ErreurSite> {
    let lire = |k: &str| {
        settings
            .get(k)
            .ok()
            .flatten()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    };
    let (Some(acces), Some(server_id)) = (lire("mozaik_access_token"), lire("server_id")) else {
        return Err(ErreurSite::NonRelie);
    };
    let base = lire("mozaik_base_url")
        .unwrap_or_else(|| SITE_PAR_DEFAUT.to_string())
        .trim_end_matches('/')
        .to_string();
    Ok(Compte {
        base,
        acces,
        liaison: lire(super::library_sync::CLE_JETON_DE_LIAISON),
        server_id,
    })
}

fn url(compte: &Compte, suite: &str) -> String {
    format!("{}{CHEMIN_API}{suite}", compte.base)
}

async fn erreur_de(r: reqwest::Response) -> ErreurSite {
    let statut = r.status().as_u16();
    let code = r
        .json::<Value>()
        .await
        .ok()
        .and_then(|v| v.get("error").and_then(Value::as_str).map(str::to_string));
    ErreurSite::Http { statut, code }
}

fn reseau(e: reqwest::Error) -> ErreurSite {
    ErreurSite::Reseau(e.without_url().to_string())
}

/// Rafraîchit le jeton du compte après un 401, une fois. Rend le compte à
/// jour, ou `None` si rien n'a pu être rafraîchi.
async fn rafraichir(settings: &SettingsRepo) -> Option<Compte> {
    use super::sso::{DEFAULT_CLIENT_ID, MozaikAuth};
    let rt = settings
        .get("mozaik_refresh_token")
        .ok()
        .flatten()
        .filter(|s| !s.is_empty())?;
    let client_id = settings
        .get("mozaik_client_id")
        .ok()
        .flatten()
        .or_else(|| std::env::var("TUNE_MOZAIK_CLIENT_ID").ok())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_CLIENT_ID.to_string());
    let base = settings.get("mozaik_base_url").ok().flatten();
    let tok = MozaikAuth::new(client_id, base.as_deref())
        .refresh_token(&rt)
        .await
        .ok()?;
    settings
        .set("mozaik_access_token", &tok.access_token)
        .ok()?;
    if let Some(n) = &tok.refresh_token {
        settings.set("mozaik_refresh_token", n).ok();
    }
    compte(settings).ok()
}

/// Exécute une requête au site ; sur 401, rafraîchit le jeton et réessaie
/// une fois.
async fn envoyer_requete<F>(
    settings: &SettingsRepo,
    faire: F,
) -> Result<reqwest::Response, ErreurSite>
where
    F: Fn(&Compte) -> reqwest::RequestBuilder,
{
    let c = compte(settings)?;
    let r = faire(&c).send().await.map_err(reseau)?;
    if r.status() != reqwest::StatusCode::UNAUTHORIZED {
        return Ok(r);
    }
    // Un 401 `server_token_invalid` porte sur le jeton de LIAISON, pas sur la
    // session : rafraîchir la session n'y changerait rien.
    let Some(c) = rafraichir(settings).await else {
        return Ok(r);
    };
    faire(&c).send().await.map_err(reseau)
}

/// Les instantanés du compte, du plus récent au plus ancien.
pub async fn lister(
    settings: &SettingsRepo,
    http: &reqwest::Client,
) -> Result<Vec<Meta>, ErreurSite> {
    let r = envoyer_requete(settings, |c| {
        http.get(url(c, ""))
            .bearer_auth(&c.acces)
            .header(reqwest::header::ACCEPT, "application/json")
            .timeout(std::time::Duration::from_secs(20))
    })
    .await?;
    if !r.status().is_success() {
        return Err(erreur_de(r).await);
    }
    let v: Value = r.json().await.map_err(|_| ErreurSite::Illisible)?;
    serde_json::from_value(v.get("backups").cloned().unwrap_or_else(|| json!([])))
        .map_err(|_| ErreurSite::Illisible)
}

/// Dépose un instantané chiffré.
pub async fn deposer(
    settings: &SettingsRepo,
    http: &reqwest::Client,
    key_id: &str,
    blob: &str,
    remplace: Option<i64>,
) -> Result<Meta, ErreurSite> {
    let label = settings
        .get("server_name")
        .ok()
        .flatten()
        .map(|s| s.trim().chars().take(120).collect::<String>())
        .filter(|s| !s.is_empty());
    let r = envoyer_requete(settings, |c| {
        let mut req = http
            .post(url(c, ""))
            .bearer_auth(&c.acces)
            .header(reqwest::header::ACCEPT, "application/json")
            .timeout(std::time::Duration::from_secs(60))
            .json(&json!({
                "server_id": c.server_id,
                "server_label": label,
                "key_id": key_id,
                "format_version": FORMAT_BLOB_VERSION,
                "replaces": remplace,
                "payload": blob,
            }));
        if let Some(j) = &c.liaison {
            req = req.header(super::library_sync::EN_TETE_JETON_DE_SERVEUR, j);
        }
        req
    })
    .await?;
    if !r.status().is_success() {
        return Err(erreur_de(r).await);
    }
    let v: Value = r.json().await.map_err(|_| ErreurSite::Illisible)?;
    serde_json::from_value(v.get("backup").cloned().unwrap_or(Value::Null))
        .map_err(|_| ErreurSite::Illisible)
}

/// Télécharge un instantané : sa description et son texte opaque.
pub async fn telecharger(
    settings: &SettingsRepo,
    http: &reqwest::Client,
    id: i64,
) -> Result<(Meta, String), ErreurSite> {
    let r = envoyer_requete(settings, |c| {
        http.get(url(c, &format!("/{id}")))
            .bearer_auth(&c.acces)
            .header(reqwest::header::ACCEPT, "application/json")
            .timeout(std::time::Duration::from_secs(60))
    })
    .await?;
    if !r.status().is_success() {
        return Err(erreur_de(r).await);
    }
    let v: Value = r.json().await.map_err(|_| ErreurSite::Illisible)?;
    let b = v.get("backup").cloned().unwrap_or(Value::Null);
    let blob = b
        .get("payload")
        .and_then(Value::as_str)
        .ok_or(ErreurSite::Illisible)?
        .to_string();
    let meta: Meta = serde_json::from_value(b).map_err(|_| ErreurSite::Illisible)?;
    Ok((meta, blob))
}

// ── La passe ────────────────────────────────────────────────────────

/// Issue d'une passe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Issue {
    /// Rien à faire (désactivée, pas de clé, rien de changé, en attente…).
    Rien(&'static str),
    /// Instantané déposé.
    Deposee(Meta),
    /// Échec, noté dans l'état.
    Echec(String),
}

/// Une passe : automatique (`manuelle = false`, soumise au délai d'attente
/// et au renouvellement quotidien) ou demandée (`manuelle = true` : part
/// tout de suite, dans un emplacement neuf, si le contenu a changé).
pub async fn passe(
    backend: &Arc<dyn DbBackend>,
    http: &reqwest::Client,
    manuelle: bool,
    maintenant: DateTime<Utc>,
) -> Issue {
    let settings = SettingsRepo::with_backend(backend.clone());
    if !manuelle && !active(&settings) {
        return Issue::Rien("disabled");
    }
    let cle = match cle_locale(&settings) {
        Ok(Some(c)) => c,
        Ok(None) => return Issue::Rien("no_key"),
        Err(e) => return Issue::Echec(e),
    };
    if compte(&settings).is_err() {
        return Issue::Rien("account_not_linked");
    }
    let etat = lire_etat(&settings);
    if !manuelle
        && settings
            .get(CLE_DERNIERE_ERREUR)
            .ok()
            .flatten()
            .is_some_and(|e| !e.is_empty())
        && date(&settings, CLE_DERNIERE_TENTATIVE)
            .is_some_and(|t| maintenant - t < Duration::minutes(REPRISE_APRES_ECHEC_MINUTES))
    {
        return Issue::Rien("backoff");
    }
    let mut contenu = match construire(backend) {
        Ok(c) => c,
        Err(e) => return Issue::Echec(e),
    };
    let emp = empreinte(&contenu);
    let remplace = if manuelle {
        if etat.derniere_empreinte.as_deref() == Some(emp.as_str()) {
            return Issue::Rien("unchanged");
        }
        None
    } else {
        match decider(&etat, &emp, maintenant) {
            Decision::Rien => {
                if etat.attente_empreinte.is_some() {
                    for k in [
                        CLE_ATTENTE_EMPREINTE,
                        CLE_ATTENTE_DEPUIS,
                        CLE_ATTENTE_PREMIERE,
                    ] {
                        settings.delete(k).ok();
                    }
                }
                return Issue::Rien("unchanged");
            }
            Decision::Noter => {
                settings.set(CLE_ATTENTE_EMPREINTE, &emp).ok();
                settings.set(CLE_ATTENTE_DEPUIS, &iso(maintenant)).ok();
                if etat.attente_premiere.is_none() {
                    settings.set(CLE_ATTENTE_PREMIERE, &iso(maintenant)).ok();
                }
                return Issue::Rien("pending");
            }
            Decision::Attendre => return Issue::Rien("pending"),
            Decision::Envoyer { remplace } => remplace,
        }
    };
    horodater(&mut contenu, maintenant);
    let blob = match chiffrer(&cle, &contenu) {
        Ok(b) => b,
        Err(e) => return Issue::Echec(e),
    };
    settings.set(CLE_DERNIERE_TENTATIVE, &iso(maintenant)).ok();
    match deposer(&settings, http, &cle.key_id, &blob, remplace).await {
        Ok(meta) => {
            settings.set(CLE_DERNIERE_EMPREINTE, &emp).ok();
            settings.set(CLE_DERNIERE, &iso(maintenant)).ok();
            settings.set(CLE_DERNIER_ID, &meta.id.to_string()).ok();
            for k in [
                CLE_ATTENTE_EMPREINTE,
                CLE_ATTENTE_DEPUIS,
                CLE_ATTENTE_PREMIERE,
                CLE_DERNIERE_ERREUR,
            ] {
                settings.delete(k).ok();
            }
            info!(
                id = meta.id,
                size = meta.size_bytes,
                remplace,
                "sauvegarde_cloud_deposee"
            );
            Issue::Deposee(meta)
        }
        Err(e) => {
            let msg = e.to_string();
            settings.set(CLE_DERNIERE_ERREUR, &msg).ok();
            warn!(error = %msg, "sauvegarde_cloud_echec");
            Issue::Echec(msg)
        }
    }
}

#[cfg(test)]
mod tests;
