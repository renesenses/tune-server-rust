use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;
use tracing::{info, warn};

const MAX_BACKUPS: usize = 5;

#[derive(Debug, Clone, Serialize)]
pub struct BackupInfo {
    pub filename: String,
    pub size: u64,
    pub created_at: String,
}

pub fn create_backup(db_path: &str) -> Option<BackupInfo> {
    let db_file = Path::new(db_path);
    if !db_file.exists() {
        return None;
    }

    let backup_dir = db_file.parent()?.join("backups");
    fs::create_dir_all(&backup_dir).ok()?;

    let stem = db_file.file_stem()?.to_str()?;
    let ext = db_file
        .extension()
        .map(|e| e.to_str().unwrap_or("db"))
        .unwrap_or("db");
    let timestamp = chrono::Local::now().format("%Y%m%d_%H%M%S");
    let backup_name = format!("{stem}_{timestamp}.{ext}");
    let backup_path = backup_dir.join(&backup_name);

    if let Err(e) = fs::copy(db_file, &backup_path) {
        warn!(error = %e, "database_backup_error");
        return None;
    }

    for suffix in ANNEXES_SQLITE {
        let wal = db_file.with_file_name(format!("{}{suffix}", db_file.file_name()?.to_str()?));
        if wal.exists() {
            let dest = backup_dir.join(format!("{backup_name}{suffix}"));
            let _ = fs::copy(&wal, &dest);
        }
    }

    info!(path = %backup_path.display(), "database_backup_created");

    prune_backups(&backup_dir, stem, ext);

    let meta = fs::metadata(&backup_path).ok()?;
    let created = meta
        .modified()
        .ok()
        .map(|t| {
            let dt: chrono::DateTime<chrono::Local> = t.into();
            dt.to_rfc3339()
        })
        .unwrap_or_default();

    Some(BackupInfo {
        filename: backup_name,
        size: meta.len(),
        created_at: created,
    })
}

pub fn list_backups(db_path: &str) -> Vec<BackupInfo> {
    let db_file = Path::new(db_path);
    let backup_dir = match db_file.parent() {
        Some(p) => p.join("backups"),
        None => return vec![],
    };
    if !backup_dir.exists() {
        return vec![];
    }

    let stem = db_file
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("tune_server");
    let ext = db_file.extension().and_then(|s| s.to_str()).unwrap_or("db");

    let pattern = format!("{stem}_");
    let suffix = format!(".{ext}");

    let mut backups: Vec<(PathBuf, BackupInfo)> = fs::read_dir(&backup_dir)
        .ok()
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let name = entry.file_name().to_str()?.to_string();
            if name.starts_with(&pattern) && name.ends_with(&suffix) {
                let meta = entry.metadata().ok()?;
                let created = meta
                    .modified()
                    .ok()
                    .map(|t| {
                        let dt: chrono::DateTime<chrono::Local> = t.into();
                        dt.to_rfc3339()
                    })
                    .unwrap_or_default();
                Some((
                    entry.path(),
                    BackupInfo {
                        filename: name,
                        size: meta.len(),
                        created_at: created,
                    },
                ))
            } else {
                None
            }
        })
        .collect();

    backups.sort_by(|a, b| b.1.filename.cmp(&a.1.filename));
    backups.into_iter().map(|(_, info)| info).collect()
}

pub fn restore_backup(db_path: &str, filename: &str) -> bool {
    let db_file = Path::new(db_path);
    let backup_dir = match db_file.parent() {
        Some(p) => p.join("backups"),
        None => return false,
    };
    let backup_path = backup_dir.join(filename);

    if !backup_path.exists() {
        return false;
    }

    if let Ok(resolved) = backup_path.canonicalize()
        && let Ok(dir_resolved) = backup_dir.canonicalize()
        && !resolved.starts_with(&dir_resolved)
    {
        warn!("path_traversal_blocked");
        return false;
    }

    match replace_database(db_path, &backup_path) {
        Ok(_) => {
            info!(backup = filename, "database_restored");
            true
        }
        Err(e) => {
            warn!(error = %e, backup = filename, "database_restore_error");
            false
        }
    }
}

/// Remplace la base active par le fichier `source`, et rend sa taille.
///
/// Extrait de [`restore_backup`] pour que l'import de base
/// (`/system/database/import`) applique un fichier reçu par le réseau, qui ne
/// vit pas dans le dossier `backups`. Les deux chemins doivent effacer le `-wal`
/// et le `-shm` de la base sortante : sans cela SQLite rejoue le journal
/// par-dessus le fichier fraîchement copié et rend un mélange des deux bases.
pub fn replace_database(db_path: &str, source: &Path) -> Result<u64, String> {
    let db_file = Path::new(db_path);
    let name = db_file
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| format!("invalid database path: {db_path}"))?;

    for suffix in ANNEXES_SQLITE {
        let side = db_file.with_file_name(format!("{name}{suffix}"));
        if side.exists() {
            let _ = fs::remove_file(&side);
        }
    }

    fs::copy(source, db_file).map_err(|e| e.to_string())
}

/// Les fichiers annexes d'une base SQLite.
///
/// Une base n'est pas UN fichier : le `-wal` porte les transactions pas encore
/// repliées dans le `.db`, le `-shm` l'index de ce journal. Les trois gestes de
/// ce module traitaient déjà les deux suffixes avec la base — [`create_backup`]
/// les copie, [`replace_database`] les efface, [`prune_backups`] les supprime —
/// mais chacun réécrivait le littéral `["-wal", "-shm"]` pour son compte.
///
/// La constante existe pour que le prochain appelant la TROUVE au lieu de
/// l'oublier. C'est exactement ce qui manquait à la migration Windows de
/// `tune-server/src/windows_migrate.rs`, qui recopiait la base de
/// *Program Files* vers `%LOCALAPPDATA%\TuneServer` avec un `fs::copy` nu :
/// la base arrivait amputée des dernières écritures encore dans son journal.
/// Le pendant macOS (#3227) l'avait écrite de son côté dans
/// `tune-server/src/config.rs` ; les deux exemplaires étaient identiques, et
/// c'est celui-ci qui reste — les deux migrations le réutilisent.
pub const ANNEXES_SQLITE: [&str; 2] = ["-wal", "-shm"];

/// Recopie une base SQLite **et ses annexes** vers `cible`, sans jamais
/// toucher à la source.
///
/// Deux garanties, parce qu'il s'agit de la donnée d'un utilisateur :
///
/// * **rien n'est détruit** — on copie, on ne déplace pas. Si quoi que ce soit
///   tourne mal ensuite, la base d'origine est encore là où elle était ;
/// * **aucun état à mi-chemin** — les fichiers sont d'abord écrits à côté de la
///   cible sous un nom temporaire, puis mis en place par des renommages faits
///   dans le même dossier. Un échec avant la fin retire les temporaires ET les
///   fichiers déjà posés, et rend l'erreur : la cible est alors exactement dans
///   l'état où elle était.
///
/// La cible n'est **pas** protégée ici : c'est à l'appelant de vérifier qu'elle
/// est absente avant d'appeler, et de journaliser la base qu'il délaisse quand
/// les deux existent — jamais d'écrasement silencieux d'une base d'utilisateur.
/// Les deux appelants le font : `config::appliquer_plan_base_macos` (#3185) et
/// `windows_migrate::appliquer_plan_migration_windows`.
///
/// Sans `cfg` et dans `tune-core` à dessein : les deux chemins qui migrent une
/// base — macOS (`~/Library/Application Support/Tune`) et Windows
/// (`%LOCALAPPDATA%\TuneServer`) — vivent derrière un `#[cfg(target_os = …)]`
/// et ne sont donc compilés sur aucune autre plateforme. Un test qui porterait
/// le même `cfg` serait vert contre rien. Ici la règle est compilée et éprouvée
/// partout ; seul le câblage — lire `%LOCALAPPDATA%`, lire `$HOME` — reste
/// sous `cfg`.
pub fn copier_base_sqlite(source: &Path, cible: &Path) -> Result<u64, String> {
    /// Défait ce qui a été posé, retire les temporaires, et rend l'erreur.
    fn renoncer(
        a_poser: &[(PathBuf, PathBuf)],
        poses: &[PathBuf],
        erreur: String,
    ) -> Result<u64, String> {
        for chemin in poses {
            let _ = fs::remove_file(chemin);
        }
        for (temporaire, _) in a_poser {
            let _ = fs::remove_file(temporaire);
        }
        Err(erreur)
    }

    let nom_source = source
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| format!("chemin de base invalide : {}", source.display()))?;
    let nom_cible = cible
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| format!("chemin de base invalide : {}", cible.display()))?;
    let marque = format!("{nom_cible}.migration-{}", std::process::id());

    // (temporaire, nom définitif)
    let mut a_poser: Vec<(PathBuf, PathBuf)> = Vec::new();
    let mut poses: Vec<PathBuf> = Vec::new();

    let temporaire = cible.with_file_name(&marque);
    if let Err(e) = fs::copy(source, &temporaire) {
        // Rien n'a encore été posé : il n'y a que ce temporaire à retirer, et
        // il peut n'avoir jamais été créé.
        let _ = fs::remove_file(&temporaire);
        return Err(format!("copie de {} : {e}", source.display()));
    }
    a_poser.push((temporaire, cible.to_path_buf()));

    for suffixe in ANNEXES_SQLITE {
        let annexe = source.with_file_name(format!("{nom_source}{suffixe}"));
        if !annexe.exists() {
            continue;
        }
        let temporaire = cible.with_file_name(format!("{marque}{suffixe}"));
        if let Err(e) = fs::copy(&annexe, &temporaire) {
            let _ = fs::remove_file(&temporaire);
            return renoncer(
                &a_poser,
                &poses,
                format!("copie de {} : {e}", annexe.display()),
            );
        }
        a_poser.push((
            temporaire,
            cible.with_file_name(format!("{nom_cible}{suffixe}")),
        ));
    }

    for (temporaire, definitif) in &a_poser {
        if let Err(e) = fs::rename(temporaire, definitif) {
            return renoncer(
                &a_poser,
                &poses,
                format!("mise en place de {} : {e}", definitif.display()),
            );
        }
        poses.push(definitif.clone());
    }

    fs::metadata(cible)
        .map(|m| m.len())
        .map_err(|e| format!("base migrée illisible à {} : {e}", cible.display()))
}

fn prune_backups(backup_dir: &Path, stem: &str, ext: &str) {
    let pattern = format!("{stem}_");
    let suffix = format!(".{ext}");

    let mut files: Vec<PathBuf> = fs::read_dir(backup_dir)
        .ok()
        .into_iter()
        .flatten()
        .filter_map(|e| {
            let e = e.ok()?;
            let name = e.file_name().to_str()?.to_string();
            if name.starts_with(&pattern) && name.ends_with(&suffix) {
                Some(e.path())
            } else {
                None
            }
        })
        .collect();

    files.sort();
    while files.len() > MAX_BACKUPS {
        if let Some(old) = files.first() {
            let _ = fs::remove_file(old);
            for s in ANNEXES_SQLITE {
                let wal = old.with_file_name(format!(
                    "{}{s}",
                    old.file_name().unwrap_or_default().to_str().unwrap_or("")
                ));
                let _ = fs::remove_file(&wal);
            }
            info!(path = %old.display(), "database_backup_pruned");
        }
        files.remove(0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_backups_empty_dir() {
        let backups = list_backups("/nonexistent/path/tune.db");
        assert!(backups.is_empty());
    }

    #[test]
    fn restore_nonexistent() {
        assert!(!restore_backup("/tmp/test.db", "nonexistent_backup.db"));
    }

    #[test]
    fn create_and_list_backup() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        fs::write(&db_path, b"test data").unwrap();

        let info = create_backup(db_path.to_str().unwrap());
        assert!(info.is_some());
        let info = info.unwrap();
        assert!(info.filename.starts_with("test_"));
        assert!(info.size > 0);

        let list = list_backups(db_path.to_str().unwrap());
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].filename, info.filename);
    }

    /// Le journal de la base SORTANTE doit disparaître avec elle.
    ///
    /// C'est la moitié invisible du remplacement : si le `-wal` survit, SQLite
    /// le rejoue par-dessus le fichier fraîchement copié et rend un mélange des
    /// deux bases. La contre-épreuve est dans le test : on vérifie que les deux
    /// annexes existaient bien AVANT l'appel, sinon leur absence après ne
    /// prouverait rien.
    #[test]
    fn replace_database_efface_wal_et_shm_de_la_base_sortante() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("tune.db");
        let wal = dir.path().join("tune.db-wal");
        let shm = dir.path().join("tune.db-shm");
        fs::write(&db_path, b"ancienne base").unwrap();
        fs::write(&wal, b"journal de l'ancienne").unwrap();
        fs::write(&shm, b"index memoire partagee").unwrap();
        assert!(wal.exists() && shm.exists(), "temoin: les annexes existent");

        let source = dir.path().join("recue.db");
        fs::write(&source, b"base importee").unwrap();

        let size = replace_database(db_path.to_str().unwrap(), &source).unwrap();

        assert_eq!(fs::read_to_string(&db_path).unwrap(), "base importee");
        assert_eq!(size, "base importee".len() as u64);
        assert!(
            !wal.exists(),
            "le -wal de la base sortante doit disparaitre"
        );
        assert!(
            !shm.exists(),
            "le -shm de la base sortante doit disparaitre"
        );
    }

    #[test]
    fn create_and_restore_backup() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        fs::write(&db_path, b"original").unwrap();

        let info = create_backup(db_path.to_str().unwrap()).unwrap();

        fs::write(&db_path, b"modified").unwrap();
        assert_eq!(fs::read_to_string(&db_path).unwrap(), "modified");

        assert!(restore_backup(db_path.to_str().unwrap(), &info.filename));
        assert_eq!(fs::read_to_string(&db_path).unwrap(), "original");
    }

    #[test]
    fn prune_keeps_max() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");

        for i in 0..8 {
            fs::write(&db_path, format!("data{i}")).unwrap();
            create_backup(db_path.to_str().unwrap());
        }

        let list = list_backups(db_path.to_str().unwrap());
        assert!(list.len() <= MAX_BACKUPS);
    }
}

// ── Encrypted backup ────────────────────────────────────────────────
//
// V2 = Argon2id key derivation + XChaCha20-Poly1305 AEAD. The AEAD tag
// authenticates the ciphertext, so a wrong password or ANY tampering fails
// loudly. The legacy V1 (time-seeded salt + single SHA-256 + repeating XOR, no
// MAC — a wrong password silently "decrypted" to garbage) is still *read* for
// backward compatibility but is never written again.

const MAGIC_V1: &[u8; 12] = b"TUNE_ENC_V1\0";
const MAGIC_V2: &[u8; 12] = b"TUNE_ENC_V2\0";
const SALT_LEN: usize = 16;
const NONCE_LEN: usize = 24; // XChaCha20 extended nonce

/// Argon2id → 32-byte key. Argon2's default is Argon2id with sane memory/time
/// costs; the salt makes precomputation useless.
fn derive_key(password: &str, salt: &[u8]) -> Result<[u8; 32], String> {
    let mut key = [0u8; 32];
    argon2::Argon2::default()
        .hash_password_into(password.as_bytes(), salt, &mut key)
        .map_err(|e| format!("key derivation failed: {e}"))?;
    Ok(key)
}

pub fn encrypt_backup(data: &[u8], password: &str) -> Vec<u8> {
    use chacha20poly1305::aead::Aead;
    use chacha20poly1305::{KeyInit, XChaCha20Poly1305, XNonce};

    let mut salt = [0u8; SALT_LEN];
    let mut nonce = [0u8; NONCE_LEN];
    getrandom::getrandom(&mut salt).expect("OS RNG unavailable");
    getrandom::getrandom(&mut nonce).expect("OS RNG unavailable");

    let key = derive_key(password, &salt).expect("argon2 key derivation");
    let cipher = XChaCha20Poly1305::new_from_slice(&key).expect("32-byte key");
    let ciphertext = cipher
        .encrypt(XNonce::from_slice(&nonce), data)
        .expect("AEAD encryption never fails for a valid key/nonce");

    let mut out = Vec::with_capacity(12 + SALT_LEN + NONCE_LEN + ciphertext.len());
    out.extend_from_slice(MAGIC_V2);
    out.extend_from_slice(&salt);
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ciphertext);
    out
}

pub fn decrypt_backup(encrypted: &[u8], password: &str) -> Result<Vec<u8>, String> {
    if encrypted.len() < 12 {
        return Err("data too short".into());
    }
    match &encrypted[..12] {
        m if m == MAGIC_V2 => decrypt_v2(encrypted, password),
        m if m == MAGIC_V1 => decrypt_v1(encrypted, password),
        _ => Err("invalid magic header".into()),
    }
}

fn decrypt_v2(encrypted: &[u8], password: &str) -> Result<Vec<u8>, String> {
    use chacha20poly1305::aead::Aead;
    use chacha20poly1305::{KeyInit, XChaCha20Poly1305, XNonce};

    let header = 12 + SALT_LEN + NONCE_LEN;
    // AEAD ciphertext carries a 16-byte Poly1305 tag.
    if encrypted.len() < header + 16 {
        return Err("truncated data".into());
    }
    let salt = &encrypted[12..12 + SALT_LEN];
    let nonce = &encrypted[12 + SALT_LEN..header];
    let ciphertext = &encrypted[header..];

    let key = derive_key(password, salt)?;
    let cipher = XChaCha20Poly1305::new_from_slice(&key).map_err(|e| e.to_string())?;
    cipher
        .decrypt(XNonce::from_slice(nonce), ciphertext)
        .map_err(|_| "wrong password or corrupted backup".to_string())
}

/// Legacy V1 reader (insecure — no integrity check). Kept only so pre-existing
/// encrypted backups can still be restored.
fn decrypt_v1(encrypted: &[u8], password: &str) -> Result<Vec<u8>, String> {
    use sha2::{Digest, Sha256};

    if encrypted.len() < 12 + SALT_LEN + 8 {
        return Err("data too short".into());
    }
    let salt = &encrypted[12..12 + SALT_LEN];
    let original_len = u64::from_le_bytes(encrypted[28..36].try_into().unwrap()) as usize;
    let cipher_data = &encrypted[36..];
    if cipher_data.len() < original_len {
        return Err("truncated data".into());
    }

    let mut hasher = Sha256::new();
    hasher.update(password.as_bytes());
    hasher.update(salt);
    let key_bytes = hasher.finalize();

    let mut decrypted = cipher_data[..original_len].to_vec();
    for (i, byte) in decrypted.iter_mut().enumerate() {
        *byte ^= key_bytes[i % key_bytes.len()];
    }
    Ok(decrypted)
}

#[cfg(test)]
mod encrypt_tests {
    use super::*;

    #[test]
    fn encrypt_decrypt_roundtrip() {
        let data = b"Hello, this is a test backup with some content!";
        let encrypted = encrypt_backup(data, "my_password");
        assert_eq!(&encrypted[..12], MAGIC_V2, "new backups use the V2 format");
        let decrypted = decrypt_backup(&encrypted, "my_password").unwrap();
        assert_eq!(decrypted, data);
    }

    #[test]
    fn wrong_password_is_rejected() {
        // AEAD: a wrong password MUST fail, never return garbage as success.
        let encrypted = encrypt_backup(b"Secret data", "correct");
        assert!(decrypt_backup(&encrypted, "wrong").is_err());
    }

    #[test]
    fn tampering_is_detected() {
        let mut encrypted = encrypt_backup(b"important bytes", "pw");
        // Flip a bit in the ciphertext body → Poly1305 tag must reject it.
        let last = encrypted.len() - 1;
        encrypted[last] ^= 0x01;
        assert!(decrypt_backup(&encrypted, "pw").is_err());
    }

    #[test]
    fn invalid_header_fails() {
        assert!(decrypt_backup(b"NOT_A_BACKUP_FILE", "password").is_err());
    }

    #[test]
    fn reads_legacy_v1_backup() {
        // Existing V1 (XOR) backups must still restore.
        use sha2::{Digest, Sha256};
        let data = b"legacy backup payload";
        let password = "old_pw";
        let salt = [7u8; SALT_LEN];
        let mut h = Sha256::new();
        h.update(password.as_bytes());
        h.update(salt);
        let key = h.finalize();
        let mut enc = data.to_vec();
        for (i, b) in enc.iter_mut().enumerate() {
            *b ^= key[i % key.len()];
        }
        let mut blob = Vec::new();
        blob.extend_from_slice(MAGIC_V1);
        blob.extend_from_slice(&salt);
        blob.extend_from_slice(&(data.len() as u64).to_le_bytes());
        blob.extend_from_slice(&enc);

        assert_eq!(decrypt_backup(&blob, password).unwrap(), data);
    }
}
