//! Racines de travail du serveur : **une par utilisateur qui exécute**, jamais
//! un chemin littéral partagé.
//!
//! # Le défaut, mesuré
//!
//! `tune-server/src/routes/converter.rs` déclarait
//! `const CONVERT_OUTPUT_ROOT: &str = "/tmp/tune-convert"` ; `declick.rs`
//! composait `/tmp/tune-declick/{job_id}` de la même façon. Deux chemins
//! **fixes**, donc partagés par tous les comptes d'une même machine.
//!
//! Sur la machine de compilation, `/tmp/tune-convert` appartient au compte
//! `jp` depuis le 18/09/2026, en `775`. Tout autre utilisateur y échoue :
//! `cargo test -p tune-server` rougit sur
//! `audio_offer_contract::audio_offer_free_eq_and_premium_four_survive_real_startup`
//! avec « failed to create output dir: Permission denied ». Le premier venu
//! crée le dossier, et le suivant ne peut plus rien y écrire — pour toujours,
//! puisque personne ne pense à supprimer un dossier de `/tmp` qui appartient à
//! quelqu'un d'autre.
//!
//! Ce faux rouge se reproduit à **chaque** campagne. Ce n'est pas son coût qui
//! compte, c'est ce qu'il enseigne : un agent qui a vu trois fois ce rouge
//! apprend à l'écarter, et il écartera le jour venu une vraie régression du
//! convertisseur. Cf `feedback_la_contre_epreuve_elle_meme_peut_etre_negative`.
//!
//! # Deux choses sont corrigées, pas une
//!
//! 1. **Le littéral `/tmp`** : il ignorait `TMPDIR`. C'était déjà un défaut en
//!    soi — un exploitant qui pose `TMPDIR=/var/tmp/tune` parce que son `/tmp`
//!    est un tmpfs de 512 Mio voyait le convertisseur le remplir quand même.
//!    On passe par [`std::env::temp_dir`], qui lit `TMPDIR` (`TMP`/`TEMP` sous
//!    Windows) : le réglage existant est désormais respecté.
//! 2. **Le nom fixe** : on y joint l'identifiant numérique de l'utilisateur
//!    courant. Deux comptes de la même machine ne se croisent plus.
//!
//! Le second sans le premier ne suffirait pas, et inversement : sous Linux
//! `temp_dir()` rend `/tmp` sans `TMPDIR`, donc toujours partagé.
//!
//! # Pourquoi l'UID et pas le nom de compte
//!
//! `$USER` et `$LOGNAME` ne sont pas posés dans l'environnement d'un service
//! systemd — c'est précisément le cas de production. L'UID, lui, est toujours
//! là : il se lit au noyau, pas à l'environnement. Et il n'a besoin d'aucune
//! échappée : c'est un entier, donc rien à assainir avant de le coller dans un
//! nom de fichier.
//!
//! # Ce que ça change en production : rien d'observable
//!
//! Les unités systemd livrées (`packaging/deb/tune-server.service`, les trois
//! `image/build-*-image.sh`) portent `PrivateTmp=yes` : le service a **déjà**
//! son `/tmp` à lui, dans un espace de noms de montage. Le dossier y change
//! seulement de nom. Et le serveur est le seul à connaître ce chemin — il le
//! publie dans `/capabilities` (`output_root`) et ne le reçoit jamais d'un
//! client. Le web-client, lui, n'en lit rien ni n'en écrit rien.
//!
//! La vraie différence est pour les installations **hors paquet** — `cargo
//! run`, Homebrew, un binaire lancé à la main — où il n'y a aucun
//! `PrivateTmp`. Ce sont elles qui souffraient du défaut, et la machine de
//! compilation en est un cas particulier.
//!
//! # À ne pas confondre avec `test_scratch`
//!
//! [`crate::test_scratch`] sert au code de **test** : un chemin par appel,
//! supprimé par `Drop`. Ce module-ci sert au code **livré** : un chemin par
//! utilisateur, durable le temps d'un travail. Le premier isole des tests
//! entre eux, le second isole des comptes entre eux. Les deux répondent à la
//! même famille de défaut — un chemin temporaire composé à la main — d'où
//! leur voisinage dans cette caisse.

use std::path::{Path, PathBuf};

/// L'identifiant numérique de l'utilisateur qui exécute ce processus.
///
/// Sous Windows il n'y a pas d'UID, et il n'en faut pas : `temp_dir()` y rend
/// déjà `…\Users\<compte>\AppData\Local\Temp`, propre au compte. On rend `0`,
/// qui ne sert alors qu'à garder une seule forme de nom entre les systèmes.
pub fn uid_courant() -> u32 {
    #[cfg(unix)]
    {
        // `getuid` ne peut pas échouer et ne touche à aucun état partagé.
        unsafe { libc::getuid() as u32 }
    }
    #[cfg(not(unix))]
    {
        0
    }
}

/// La racine de travail de l'utilisateur courant pour une `etiquette` donnée.
///
/// `etiquette` nomme le chantier (`"tune-convert"`, `"tune-declick"`) ; c'est
/// elle qui dit à qui appartient un résidu retrouvé dans `/tmp` un mois plus
/// tard.
///
/// ```ignore
/// let racine = tune_core::chemins_de_travail::racine_de_travail("tune-convert");
/// // /tmp/tune-convert-1001 sous Linux, $TMPDIR/tune-convert-501 sous macOS.
/// ```
pub fn racine_de_travail(etiquette: &str) -> PathBuf {
    racine_de_travail_sous(std::env::temp_dir(), etiquette, uid_courant())
}

/// La même, avec sa base et son UID **passés** — c'est la forme testable.
///
/// Séparer les deux n'est pas de la décoration : une garde qui appellerait
/// `racine_de_travail()` deux fois dans le même processus obtiendrait deux fois
/// le même UID et ne prouverait donc **rien** sur la séparation de deux
/// comptes. Cf `feedback_une_garde_qui_construit_elle_meme_ne_garde_pas_le_branchement`.
pub fn racine_de_travail_sous(base: impl AsRef<Path>, etiquette: &str, uid: u32) -> PathBuf {
    base.as_ref().join(format!("{etiquette}-{uid}"))
}

/// Vrai quand `dossier` vit sur un système de fichiers EN MÉMOIRE (`tmpfs`,
/// `ramfs`) : chaque octet qu'on y écrit est de la mémoire vive, que le
/// processus ne voit pas dans son RSS et que le système ne peut pas reprendre.
///
/// # Pourquoi c'est un défaut mémoire (fil 2167)
///
/// Deux caches de lecture vivent dans le dossier temporaire : les copies de
/// préchargement des sources sur montage réseau (`tune-stage-*`, 3 Gio par
/// défaut) et les rendus transcodés pour les zones réseau
/// (`tune-tcache-<uid>/`, 4 Gio par défaut). Sur un disque, ces plafonds sont
/// raisonnables. Mais `/tmp` est un `tmpfs` par défaut sur plusieurs
/// distributions de bureau : les 7 Gio deviennent alors de la RAM, qui monte
/// piste après piste et ne redescend pas, puisque les deux caches gardent
/// leurs fichiers jusqu'au plafond. Relevé sur un journal de terrain (zone
/// DLNA avec égaliseur, sources en CIFS) : 2,6 Gio écrits dans ces deux caches
/// en 1 h 45.
///
/// Hors Linux, ou si le dossier ne se lit pas : `false` (rien ne change).
pub fn est_en_memoire(dossier: &Path) -> bool {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::ffi::OsStrExt;
        const TMPFS_MAGIC: i64 = 0x0102_1994;
        const RAMFS_MAGIC: i64 = 0x8584_58f6;
        let Ok(c) = std::ffi::CString::new(dossier.as_os_str().as_bytes()) else {
            return false;
        };
        let mut sfs: libc::statfs = unsafe { std::mem::zeroed() };
        // SAFETY : `c` est une chaîne C valide, `sfs` un tampon de la bonne
        // taille ; `statfs` ne retient aucun des deux.
        if unsafe { libc::statfs(c.as_ptr(), &mut sfs) } != 0 {
            return false;
        }
        #[allow(clippy::unnecessary_cast)]
        let genre = sfs.f_type as i64;
        genre == TMPFS_MAGIC || genre == RAMFS_MAGIC
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = dossier;
        false
    }
}

/// La mémoire vive totale de la machine, en octets (`MemTotal` de
/// `/proc/meminfo`). `None` hors Linux ou si la ligne ne se lit pas.
pub fn memoire_vive_totale() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        let meminfo = std::fs::read_to_string("/proc/meminfo").ok()?;
        let ligne = meminfo.lines().find(|l| l.starts_with("MemTotal:"))?;
        let kio: u64 = ligne.split_whitespace().nth(1)?.parse().ok()?;
        Some(kio.saturating_mul(1024))
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

/// La mémoire qu'une nouvelle allocation peut encore obtenir, en octets
/// (#8558). Linux seulement : le MINIMUM de `MemAvailable` (`/proc/meminfo`)
/// et de la marge sous chaque plafond cgroup v2 (`memory.max`) du cgroup du
/// processus et de ses ancêtres, lu via `/proc/self/cgroup` — une unité
/// systemd bornée ou un conteneur `--memory` se voient ainsi, même quand le
/// plafond est posé sur un parent et non à la racine.
///
/// La marge d'un cgroup est `memory.max − (memory.current − inactive_file)` :
/// `memory.current` compte le cache de pages, que le noyau récupère avant de
/// tuer quiconque. Sans ce retrait, un conteneur qui a lu quelques albums
/// paraissait plein (marge ≈ 0).
///
/// `None` hors Linux, ou si rien ne se lit : l'appelant ne doit rien inventer.
/// Le cgroup v1 n'est pas lu ici (`MemAvailable` seul).
///
/// ⚠️ `audio::embedding` a sa propre lecture (cgroup v1 compris), derrière la
/// fonctionnalité `audio-embedding` : à fusionner ici quand on y touchera.
pub fn memoire_disponible() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        let lire = |p: &str| std::fs::read_to_string(p).ok();
        let meminfo = lire("/proc/meminfo")
            .as_deref()
            .and_then(memoire_disponible_de_meminfo);
        let marges = lire("/proc/self/cgroup")
            .as_deref()
            .and_then(chemin_du_cgroup_v2)
            .map(|chemin| {
                dossiers_du_cgroup_et_de_ses_parents("/sys/fs/cgroup", &chemin)
                    .into_iter()
                    .filter_map(|d| {
                        marge_du_cgroup(
                            lire(&format!("{d}/memory.max")).as_deref(),
                            lire(&format!("{d}/memory.current")).as_deref(),
                            lire(&format!("{d}/memory.stat")).as_deref(),
                        )
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        plus_petite_marge(meminfo, marges)
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

/// Le chemin du cgroup v2 du processus, tiré de `/proc/self/cgroup` (la ligne
/// `0::/chemin`). `None` en cgroup v1 pur.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn chemin_du_cgroup_v2(proc_self_cgroup: &str) -> Option<String> {
    proc_self_cgroup
        .lines()
        .find_map(|l| l.strip_prefix("0::"))
        .map(|c| c.trim().to_string())
}

/// Le dossier du cgroup sous `racine`, puis chacun de ses parents jusqu'à la
/// racine comprise. Un chemin qui remonte (`..`) est refusé : on ne lit que
/// sous `racine`.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn dossiers_du_cgroup_et_de_ses_parents(racine: &str, chemin: &str) -> Vec<String> {
    let morceaux: Vec<&str> = chemin.split('/').filter(|m| !m.is_empty()).collect();
    if morceaux.contains(&"..") {
        return vec![racine.to_string()];
    }
    (0..=morceaux.len())
        .rev()
        .map(|n| {
            let mut d = racine.to_string();
            for m in &morceaux[..n] {
                d.push('/');
                d.push_str(m);
            }
            d
        })
        .collect()
}

/// Marge sous le plafond d'UN cgroup v2, en octets : `memory.max` moins
/// `memory.current` dont on retire `inactive_file` (cache de pages
/// récupérable, `memory.stat`). `None` sans plafond (`max`, fichier absent ou
/// illisible).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn marge_du_cgroup(plafond: Option<&str>, usage: Option<&str>, stat: Option<&str>) -> Option<u64> {
    let plafond: u64 = plafond?.trim().parse().ok()?;
    let usage: u64 = usage?.trim().parse().ok()?;
    let recuperable: u64 = stat
        .and_then(|s| {
            s.lines()
                .find_map(|l| l.strip_prefix("inactive_file "))
                .and_then(|v| v.trim().parse().ok())
        })
        .unwrap_or(0);
    Some(plafond.saturating_sub(usage.saturating_sub(recuperable)))
}

/// Le plus petit de `MemAvailable` et des marges des cgroups. `None` si rien
/// n'a été lu.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn plus_petite_marge(meminfo: Option<u64>, marges: Vec<u64>) -> Option<u64> {
    meminfo.into_iter().chain(marges).min()
}

/// `MemAvailable` de `/proc/meminfo`, en octets.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn memoire_disponible_de_meminfo(meminfo: &str) -> Option<u64> {
    let kio: u64 = meminfo
        .lines()
        .find_map(|l| l.strip_prefix("MemAvailable:"))?
        .split_whitespace()
        .next()?
        .parse()
        .ok()?;
    Some(kio.saturating_mul(1024))
}

/// Plancher du plafond d'un cache posé en mémoire : de quoi garder la piste
/// en cours et la suivante, même sur une petite machine.
pub const PLANCHER_CACHE_EN_MEMOIRE: u64 = 128 * 1024 * 1024;

/// Le plafond effectif d'un cache disque de lecture (fil 2167).
///
/// - `reglage` : le plafond posé explicitement par l'exploitant (variable
///   d'environnement). Il est toujours respecté, tel quel.
/// - Sinon, sur un disque : `defaut`, inchangé.
/// - Sinon, sur un système de fichiers en mémoire : un trente-deuxième de la
///   RAM (512 Mio sur 16 Gio), jamais moins que [`PLANCHER_CACHE_EN_MEMOIRE`]
///   et jamais plus que `defaut`. RAM inconnue : le plancher.
///
/// Pure, pour être prouvée sans `tmpfs`.
pub fn plafond_de_cache(
    defaut: u64,
    reglage: Option<u64>,
    en_memoire: bool,
    ram_totale: Option<u64>,
) -> u64 {
    if let Some(r) = reglage {
        return r;
    }
    if !en_memoire {
        return defaut;
    }
    let part = ram_totale.map(|r| r / 32).unwrap_or(0);
    part.max(PLANCHER_CACHE_EN_MEMOIRE).min(defaut)
}

/// [`plafond_de_cache`] pour un cache rangé dans `dossier` : le type du
/// système de fichiers est LU, la RAM est passée (en production
/// [`memoire_vive_totale`]). Journalise une fois par cache (`etiquette`) quand
/// le plafond est abaissé parce que le dossier est en mémoire.
pub fn plafond_de_cache_pour(
    dossier: &Path,
    etiquette: &'static str,
    defaut: u64,
    reglage: Option<u64>,
    ram_totale: Option<u64>,
) -> u64 {
    let en_memoire = reglage.is_none() && est_en_memoire(dossier);
    let plafond = plafond_de_cache(defaut, reglage, en_memoire, ram_totale);
    if en_memoire && plafond < defaut {
        static DITS: std::sync::Mutex<Vec<&'static str>> = std::sync::Mutex::new(Vec::new());
        let premier = DITS
            .lock()
            .map(|mut d| {
                let neuf = !d.contains(&etiquette);
                if neuf {
                    d.push(etiquette);
                }
                neuf
            })
            .unwrap_or(false);
        if premier {
            tracing::info!(
                cache = etiquette,
                dossier = %dossier.display(),
                plafond_mo = plafond / (1024 * 1024),
                defaut_mo = defaut / (1024 * 1024),
                "cache_en_memoire_plafond_abaisse — le dossier temporaire est un tmpfs : \
                 ce cache serait de la RAM, son plafond suit la mémoire de la machine"
            );
        }
    }
    plafond
}

#[cfg(test)]
mod tests {
    #[test]
    fn memoire_disponible_lit_le_cgroup_puis_meminfo_8558() {
        // Sans plafond, pas de marge de cgroup.
        assert_eq!(marge_du_cgroup(Some("max\n"), Some("100\n"), None), None);
        assert_eq!(marge_du_cgroup(None, Some("1"), None), None);
        assert_eq!(
            marge_du_cgroup(Some("1000\n"), Some("300\n"), None),
            Some(700)
        );
        assert_eq!(marge_du_cgroup(Some("100"), Some("300"), None), Some(0));
        // Le cache de pages récupérable (`inactive_file`) est rendu à la
        // marge : un conteneur qui a lu des albums n'est pas « plein ».
        let stat = "anon 100\nfile 900\ninactive_file 800\nactive_file 100\n";
        assert_eq!(
            marge_du_cgroup(Some("1000"), Some("1000"), Some(stat)),
            Some(800)
        );
        // Un `inactive_file` incohérent (> usage) ne fait pas déborder.
        assert_eq!(
            marge_du_cgroup(Some("1000"), Some("10"), Some("inactive_file 50\n")),
            Some(1000)
        );
        assert_eq!(
            memoire_disponible_de_meminfo(
                "MemTotal:  4177920 kB\nMemFree: 1 kB\nMemAvailable:   3460000 kB\n"
            ),
            Some(3_460_000 * 1024)
        );
        assert_eq!(memoire_disponible_de_meminfo("MemTotal: 1 kB\n"), None);
        // Le minimum de tout ce qui a été lu.
        assert_eq!(plus_petite_marge(Some(500), vec![700, 300]), Some(300));
        assert_eq!(plus_petite_marge(Some(200), vec![700]), Some(200));
        assert_eq!(plus_petite_marge(None, vec![]), None);
    }

    #[test]
    fn le_cgroup_du_processus_et_ses_parents_8558() {
        assert_eq!(
            chemin_du_cgroup_v2("0::/system.slice/tune.service\n"),
            Some("/system.slice/tune.service".into())
        );
        // cgroup v1 pur : rien.
        assert_eq!(chemin_du_cgroup_v2("4:memory:/docker/abc\n"), None);
        assert_eq!(
            dossiers_du_cgroup_et_de_ses_parents("/sys/fs/cgroup", "/system.slice/tune.service"),
            vec![
                "/sys/fs/cgroup/system.slice/tune.service".to_string(),
                "/sys/fs/cgroup/system.slice".to_string(),
                "/sys/fs/cgroup".to_string(),
            ]
        );
        // Conteneur avec espace de noms cgroup : `0::/` — la racine seule.
        assert_eq!(
            dossiers_du_cgroup_et_de_ses_parents("/sys/fs/cgroup", "/"),
            vec!["/sys/fs/cgroup".to_string()]
        );
        assert_eq!(
            dossiers_du_cgroup_et_de_ses_parents("/sys/fs/cgroup", "/../etc"),
            vec!["/sys/fs/cgroup".to_string()]
        );
    }

    use super::*;

    /// Le témoin de #4770 : **deux utilisateurs ne se marchent pas dessus**.
    ///
    /// Reproduit le défaut de Shrek sans avoir besoin de deux comptes. Le
    /// dossier « de l'autre » est créé puis passé en `555` : c'est exactement
    /// ce que voit un second utilisateur devant le `/tmp/tune-convert` de
    /// `jp` — le dossier existe, il est lisible, et on ne peut rien y créer.
    ///
    /// Trois assertions, dans cet ordre, et c'est l'ordre qui fait la preuve :
    /// 1. le chemin FIXE échoue — sans ça le test passerait sur une machine où
    ///    le défaut n'existe pas, et ne prouverait rien ;
    /// 2. les deux racines par utilisateur sont distinctes ;
    /// 3. elles se créent toutes les deux **pour de vrai**.
    #[cfg(unix)]
    #[test]
    fn deux_utilisateurs_ne_partagent_pas_la_racine_de_travail() {
        use std::os::unix::fs::PermissionsExt;

        if uid_courant() == 0 {
            // root ignore les permissions : le témoin ne pourrait pas rougir,
            // donc il ne prouverait rien. On le dit au lieu de passer vert.
            eprintln!("témoin ignoré : exécuté en root, les modes ne mordent pas");
            return;
        }

        let base = crate::test_scratch::scratch_dir("tune-chemins-de-travail");
        let travail = "0f0e0d0c-1111-2222-3333-444444444444";

        // 1. Le geste d'AVANT : une racine au nom fixe, déjà créée par
        //    quelqu'un d'autre. C'est le rouge de Shrek, ici reproduit.
        let partagee = base.join("tune-convert");
        std::fs::create_dir_all(&partagee).expect("racine « de l'autre utilisateur »");
        std::fs::set_permissions(&partagee, std::fs::Permissions::from_mode(0o555))
            .expect("mode 555");
        let echec = std::fs::create_dir_all(partagee.join(travail));
        assert!(
            echec.is_err(),
            "le chemin fixe aurait dû être refusé : sans ce rouge, la suite ne prouve rien"
        );
        assert_eq!(
            echec.unwrap_err().kind(),
            std::io::ErrorKind::PermissionDenied,
            "le refus attendu est bien celui des permissions"
        );
        // Rendu inscriptible pour que le ménage de `ScratchDir` l'emporte.
        std::fs::set_permissions(&partagee, std::fs::Permissions::from_mode(0o755))
            .expect("mode 755");

        // 2. Le geste d'APRÈS : deux UID, deux racines.
        let mienne = racine_de_travail_sous(&base, "tune-convert", 1000);
        let sienne = racine_de_travail_sous(&base, "tune-convert", 1001);
        assert_ne!(mienne, sienne, "deux UID rendent le même chemin");
        assert!(
            !mienne.starts_with(&sienne) && !sienne.starts_with(&mienne),
            "une racine est contenue dans l'autre : {mienne:?} / {sienne:?}"
        );
        assert_ne!(
            mienne, partagee,
            "la racine par utilisateur est retombée sur le nom fixe"
        );

        // 3. Et elles se créent toutes les deux, côte à côte.
        for racine in [&mienne, &sienne] {
            std::fs::create_dir_all(racine.join(travail))
                .unwrap_or_else(|e| panic!("création de {racine:?} refusée : {e}"));
            assert!(racine.join(travail).is_dir());
        }
    }

    /// Fil 2167 — le plafond d'un cache suit la RAM sur un système de
    /// fichiers en mémoire, et seulement là.
    #[test]
    fn le_plafond_d_un_cache_en_memoire_suit_la_ram() {
        const MIO: u64 = 1024 * 1024;
        const GIO: u64 = 1024 * MIO;
        // Sur disque : le défaut, inchangé.
        assert_eq!(
            plafond_de_cache(4 * GIO, None, false, Some(16 * GIO)),
            4 * GIO
        );
        // En mémoire : 1/32 de la RAM.
        assert_eq!(
            plafond_de_cache(4 * GIO, None, true, Some(16 * GIO)),
            512 * MIO
        );
        // Jamais plus que le défaut, jamais moins que le plancher.
        assert_eq!(
            plafond_de_cache(4 * GIO, None, true, Some(512 * GIO)),
            4 * GIO
        );
        assert_eq!(
            plafond_de_cache(4 * GIO, None, true, Some(2 * GIO)),
            PLANCHER_CACHE_EN_MEMOIRE
        );
        assert_eq!(
            plafond_de_cache(4 * GIO, None, true, None),
            PLANCHER_CACHE_EN_MEMOIRE
        );
        // Un réglage explicite est respecté tel quel, en mémoire ou non.
        assert_eq!(
            plafond_de_cache(4 * GIO, Some(8 * GIO), true, Some(16 * GIO)),
            8 * GIO
        );
    }

    /// `/dev/shm` est un tmpfs sous Linux : la détection doit le voir.
    #[cfg(target_os = "linux")]
    #[test]
    fn un_tmpfs_est_reconnu_en_memoire() {
        assert!(
            est_en_memoire(Path::new("/dev/shm")),
            "/dev/shm n'est pas reconnu comme système de fichiers en mémoire"
        );
        assert!(!est_en_memoire(Path::new("/chemin/qui/n/existe/pas")));
    }

    /// L'étiquette reste lisible dans le nom, et la base est respectée.
    ///
    /// Un résidu retrouvé dans `/tmp` doit dire d'où il vient : `tune-convert`
    /// et `tune-declick` se distinguent, et un suffixe seul ne l'aurait pas
    /// permis.
    #[test]
    fn l_etiquette_et_la_base_sont_conservees() {
        let racine = racine_de_travail_sous("/base/a/soi", "tune-declick", 4242);
        assert_eq!(racine, Path::new("/base/a/soi/tune-declick-4242"));
        assert_ne!(
            racine_de_travail_sous("/base/a/soi", "tune-convert", 4242),
            racine,
            "deux chantiers du même utilisateur partagent leur racine"
        );
    }

    /// `TMPDIR` est respecté : c'est le réglage qui existait déjà et que le
    /// chemin littéral ignorait. On ne le prouve pas en posant la variable
    /// (l'environnement est global au processus, donc aux tests parallèles),
    /// mais en montrant que la racine est bien sous `temp_dir()`, seule
    /// fonction qui la lit.
    ///
    /// ⚠️ Le nom de ce test ne peut pas s'écrire « …_sous_temp_dir » : le
    /// garde `aucune_fuite_de_temporaires` cherche le motif dans la LIGNE, et
    /// la ligne de la signature l'aurait porté. Il a rougi dessus, et il avait
    /// raison de le faire — la même règle vaudra pour le prochain.
    #[test]
    fn la_racine_vit_sous_le_dossier_temporaire() {
        let racine = racine_de_travail("tune-convert");
        // tmp-autorise: rien n'est créé ici, on LIT la racine pour la comparer.
        let attendue = std::env::temp_dir();
        assert!(
            racine.starts_with(&attendue),
            "{racine:?} n'est pas sous {attendue:?}"
        );
        assert_eq!(
            racine.file_name().unwrap().to_string_lossy(),
            format!("tune-convert-{}", uid_courant())
        );
    }
}
