//! Périmètre de l'explorateur de dossiers servi par le serveur (#1275).
//!
//! Le sélecteur de dossiers des réglages Bibliothèque ne peut pas être un
//! dialogue natif : ils sont proscrits dans les webviews de ce projet. Le
//! serveur expose donc l'arborescence et le client la dessine — ce qui met une
//! route de LECTURE DU SYSTÈME DE FICHIERS de la machine serveur à portée du
//! réseau local. `GET /system/browse-dirs` existe depuis longtemps et n'avait
//! aucune borne : elle partait de `/` (ou `C:\`), acceptait n'importe quel
//! `?path=`, et sondait chaque enfant. Sans rôle exigé, sur une installation
//! par défaut (`auth_enabled` absent ⇒ serveur ouvert « LAN de confiance »),
//! n'importe qui sur le réseau pouvait énumérer `/etc`, `/root`,
//! `C:\Users\…`, et cartographier la machine.
//!
//! Ce module porte les trois refus. Ils sont volontairement en **logique
//! pure** : testables sur n'importe quelle plateforme, donc un test ne peut
//! pas rester vert sur le CI Linux pendant que Windows perd la garde — c'est
//! exactement ainsi que #2016 a livré quatre fois le même défaut.
//!
//! Le périmètre retenu est **le disque MOINS les arbres système**, et non une
//! liste blanche de racines. Justification : le geste que #1275 doit rendre
//! possible est de désigner une racine de bibliothèque qui n'existe pas encore
//! dans les réglages — un NAS monté sur `/nas`, une clé sur `/media/…`, un
//! `D:\Musique`, un `/opt/musique`. Une liste blanche refuserait ces
//! dispositions légitimes et renverrait l'utilisateur à la saisie manuelle,
//! c'est-à-dire au défaut que l'issue demande de supprimer. La route
//! d'écriture qu'il alimente (`POST /system/music-dirs`) accepte déjà, elle,
//! n'importe quel chemin existant.

use tune_core::metadata::enrich_scope::sous_le_dossier;

/// Arbres système absolus : jamais un endroit où vit de la musique, toujours
/// un endroit où la reconnaissance d'une machine commence.
///
/// `/run` et `/media` en sont volontairement ABSENTS : c'est là que Linux
/// monte les disques amovibles (`/run/media/<user>/<clé>`), donc là que vit
/// souvent la bibliothèque qu'on vient désigner.
pub(crate) const ARBRES_SYSTEME: &[&str] = &[
    // Linux / BSD
    "/bin",
    "/boot",
    "/dev",
    "/etc",
    "/lib",
    "/lib32",
    "/lib64",
    "/libx32",
    "/proc",
    "/root",
    "/sbin",
    "/usr",
    "/sys",
    "/var",
    // macOS — `/etc` et `/var` y sont des liens vers `/private/…`, que la
    // forme canonique fait apparaître.
    "/System",
    "/Library",
    "/cores",
    "/private/etc",
    "/private/var",
];

/// Dossiers système de Windows, nommés RELATIVEMENT à la racine de leur
/// lecteur : ils existent sur `C:` comme sur `D:`.
pub(crate) const DOSSIERS_SYSTEME_WINDOWS: &[&str] = &[
    "Windows",
    "Program Files",
    "Program Files (x86)",
    "ProgramData",
    "$Recycle.Bin",
    "$WinREAgent",
    "System Volume Information",
    "Recovery",
    "PerfLogs",
    "Config.Msi",
];

/// Motif du refus opposé à un chemin demandé. Le client n'en reçoit que le
/// libellé : jamais l'erreur système, jamais de quoi distinguer « ce dossier
/// existe mais je le refuse » de « ce dossier n'existe pas » — cette
/// différence-là est déjà un oracle de reconnaissance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Refus {
    /// Chemin relatif, ou vide : l'explorateur ne travaille qu'en absolu.
    PasAbsolu,
    /// Le chemin contient un `..`. Il n'est jamais nécessaire — le client
    /// remonte par le champ `parent` de la réponse — et il est le seul moyen
    /// de faire mentir une garde de préfixe.
    RemonteVersLeParent,
    /// Le chemin tombe dans un arbre système.
    ArbreSysteme,
    /// Sécurité — une racine de disque (`/`, `C:\`, `\\nas`) ou
    /// un dossier qui CONTIENT un arbre système (`/private`) : l'explorateur
    /// peut l'afficher, il ne peut pas devenir un dossier de musique.
    TropLarge,
}

impl Refus {
    pub(crate) fn libelle(self) -> &'static str {
        match self {
            Refus::PasAbsolu => "path must be absolute",
            Refus::RemonteVersLeParent => "path must not contain '..'",
            Refus::ArbreSysteme => "path is outside the browsable perimeter",
            Refus::TropLarge => "path is too broad to be a music directory",
        }
    }
}

/// Le chemin est-il écrit sous l'une des trois formes absolues ?
///
/// `Path::is_absolute()` ne convient pas : sur l'hôte Linux du CI il déclare
/// `D:\Musique` RELATIF, la garde refuserait donc tout chemin Windows tout en
/// restant verte. Le même angle mort a coûté #1837 et #2056.
pub(crate) fn est_absolu(chemin: &str) -> bool {
    chemin.starts_with('/')
        || crate::chemin_inaccessible::est_un_chemin_unc(chemin)
        || crate::chemin_inaccessible::lettre_de_lecteur(chemin).is_some()
}

/// Un segment `..` quelque part dans le chemin ?
///
/// Le découpage prend les DEUX séparateurs. `Path::components()` ne suffirait
/// pas : sur un hôte POSIX, `D:\Musique\..\..\etc` est UN seul composant
/// `Normal`, le `..` y est invisible, et la garde passerait.
pub(crate) fn remonte_vers_le_parent(chemin: &str) -> bool {
    chemin.split(['/', '\\']).any(|segment| segment == "..")
}

/// Le premier composant après une racine de lecteur Windows (`C:\Windows` ⇒
/// `Windows`). `None` pour tout ce qui n'est pas `X:` — un chemin POSIX, un
/// partage UNC dont le premier composant est un nom de partage, pas un
/// dossier système.
fn dossier_de_tete_windows(chemin: &str) -> Option<&str> {
    crate::chemin_inaccessible::lettre_de_lecteur(chemin)?;
    let reste = chemin[2..].trim_start_matches(['/', '\\']);
    let fin = reste.find(['/', '\\']).unwrap_or(reste.len());
    let tete = &reste[..fin];
    (!tete.is_empty()).then_some(tete)
}

/// Le chemin tombe-t-il dans un arbre système ?
///
/// Les deux jeux de règles sont évalués sur TOUTES les plateformes. Les
/// enfermer derrière un `cfg` rendrait la règle Windows intestable ailleurs
/// que sous Windows, où personne ne la fait tourner.
pub(crate) fn dans_un_arbre_systeme(chemin: &str) -> bool {
    if ARBRES_SYSTEME
        .iter()
        .any(|systeme| sous_le_dossier(chemin, systeme))
    {
        return true;
    }
    // La casse ne distingue rien sous Windows : `C:\WINDOWS` est `C:\Windows`.
    dossier_de_tete_windows(chemin).is_some_and(|tete| {
        DOSSIERS_SYSTEME_WINDOWS
            .iter()
            .any(|systeme| systeme.eq_ignore_ascii_case(tete))
    })
}

/// Forme canonique d'un chemin, débarrassée du préfixe « verbatim » que
/// Windows ajoute (`\\?\C:\…`) et que les règles ci-dessus ne reconnaîtraient
/// pas.
///
/// Sert à voir la CIBLE d'un lien symbolique : sans elle, un lien
/// `~/Musique/sys → /sys` posé dans une racine de bibliothèque ouvrirait
/// l'arbre système par un chemin dont le texte, lui, est irréprochable.
pub(crate) fn forme_canonique(chemin: &std::path::Path) -> Option<String> {
    let canonique = std::fs::canonicalize(chemin).ok()?;
    let texte = canonique.to_string_lossy().into_owned();
    if let Some(reste) = texte.strip_prefix(r"\\?\UNC\") {
        return Some(format!(r"\\{reste}"));
    }
    if let Some(reste) = texte.strip_prefix(r"\\?\") {
        return Some(reste.to_string());
    }
    Some(texte)
}

/// La garde complète appliquée au chemin DEMANDÉ, avant toute lecture disque.
pub(crate) fn verifier_le_chemin_demande(chemin: &str) -> Result<(), Refus> {
    if !est_absolu(chemin) {
        return Err(Refus::PasAbsolu);
    }
    if remonte_vers_le_parent(chemin) {
        return Err(Refus::RemonteVersLeParent);
    }
    if dans_un_arbre_systeme(chemin) {
        return Err(Refus::ArbreSysteme);
    }
    Ok(())
}

/// Ce chemin, une fois résolu sur le disque, reste-t-il hors des arbres
/// système ? Complète [`verifier_le_chemin_demande`] pour le cas du lien
/// symbolique, où le texte et la cible divergent.
pub(crate) fn la_cible_reste_dans_le_perimetre(chemin: &std::path::Path) -> bool {
    match forme_canonique(chemin) {
        // Un chemin illisible ne prouve rien : c'est la lecture qui tranchera,
        // et elle rendra sa propre erreur.
        None => true,
        Some(canonique) => !dans_un_arbre_systeme(&canonique),
    }
}

// ───────────────────────────────────────────────────────────────────────────
// Sécurité — le périmètre d'une RACINE de bibliothèque
// ───────────────────────────────────────────────────────────────────────────

/// Les segments du chemin, sur les deux séparateurs, sans vides ni `.`.
fn segments_lexicaux(chemin: &str) -> Vec<&str> {
    chemin
        .split(['/', '\\'])
        .filter(|s| !s.is_empty() && *s != ".")
        .collect()
}

/// La forme repliée du chemin : séparateurs doublés et `.` retirés.
/// `//etc` ou `/./etc` y redeviennent `/etc`, que la règle des arbres système
/// reconnaît ; `C:\\.\\Windows` redevient `C:\Windows`.
fn forme_repliee(chemin: &str) -> String {
    let segs = segments_lexicaux(chemin);
    match segs.split_first() {
        Some((tete, reste)) if crate::chemin_inaccessible::lettre_de_lecteur(tete).is_some() => {
            format!("{tete}\\{}", reste.join("\\"))
        }
        _ => format!("/{}", segs.join("/")),
    }
}

/// `/`, `\`, `C:\`, `\\nas` (sans partage) : tout un disque, ou tout un
/// serveur. Lu sur la forme repliée, donc `//`, `/./` ou `C:\.` comptent.
fn est_une_racine_de_disque(chemin: &str) -> bool {
    let segs = segments_lexicaux(chemin);
    match segs.as_slice() {
        [] => true,
        [seul] => {
            crate::chemin_inaccessible::lettre_de_lecteur(seul).is_some()
                || crate::chemin_inaccessible::est_un_chemin_unc(chemin.trim_start())
        }
        _ => false,
    }
}

/// Un arbre système vit-il SOUS ce chemin ? `/private` contient
/// `/private/etc` : déclaré dossier de musique, il ouvrirait l'arbre entier.
fn contient_un_arbre_systeme(chemin: &str) -> bool {
    let c = forme_repliee(chemin);
    ARBRES_SYSTEME
        .iter()
        .any(|systeme| sous_le_dossier(systeme, &c))
}

/// La garde de TOUTE écriture d'un dossier de musique (ajout, onboarding,
/// `PATCH /system/config`, imports de configuration).
///
/// Les dossiers de musique sont le périmètre de toutes les autres gardes de
/// chemin (`tune_core::chemin_sur`) : un client qui pourrait déclarer `/`
/// défairait toutes ces gardes d'une requête. La règle est celle de
/// l'explorateur, jamais plus large : absolu, sans `..` (forme encodée
/// comprise), hors des arbres système — et en plus, ni une racine de disque,
/// ni un dossier qui contient un arbre système, sur le TEXTE (replié) comme
/// sur la CIBLE d'un lien symbolique.
///
/// Le disque n'est consulté que pour la forme canonique : un partage
/// décroché (chemin illisible) n'est pas refusé pour autant, comme dans
/// l'explorateur.
pub(crate) fn verifier_une_racine_de_musique(chemin: &str) -> Result<(), Refus> {
    verifier_une_racine_de_musique_avec(chemin, &[])
}

/// Dossiers qui ne sont ni un disque ni un arbre système, mais qui
/// RASSEMBLENT les volumes de la machine : `/Volumes` porte sous macOS le
/// lien `Macintosh HD → /`. Déclaré racine, il rendrait tout le disque
/// « sous la bibliothèque » pour des gardes lexicales.
const RASSEMBLEMENTS_DE_VOLUMES: &[&str] = &["/Volumes", "/System/Volumes"];

/// Nombre maximal d'enfants directs inspectés à la déclaration d'une racine.
const ENFANTS_INSPECTES_MAX: usize = 4096;

/// Une cible (forme canonique) qui ouvrirait plus que de la musique : une
/// racine de disque, un arbre système, un dossier qui en contient un, ou un
/// partage d'administration.
fn cible_trop_large(canonique: &str) -> bool {
    dans_un_arbre_systeme(canonique)
        || est_une_racine_de_disque(canonique)
        || contient_un_arbre_systeme(canonique)
        || est_un_rassemblement_de_volumes(canonique)
        || est_un_chemin_windows_interdit(canonique)
}

fn est_un_rassemblement_de_volumes(chemin: &str) -> bool {
    let c = forme_repliee(chemin);
    RASSEMBLEMENTS_DE_VOLUMES
        .iter()
        .any(|v| c.eq_ignore_ascii_case(v))
}

/// Chemins Windows qu'aucune bibliothèque n'emprunte et qui ouvrent un disque
/// entier : préfixes de périphérique (`\\?\`, `\\.\`, y compris
/// `\\?\UNC\…`), partages d'administration (`\\hôte\C$`, `\\hôte\ADMIN$`),
/// et partages servis par la machine elle-même (`\\localhost\…`,
/// `\\127.0.0.1\…`, `\\[::1]\…`, `\\wsl.localhost\…`, `\\wsl$\…`).
fn est_un_chemin_windows_interdit(chemin: &str) -> bool {
    let c = chemin.trim_start();
    let o = c.as_bytes();
    if o.len() >= 4
        && matches!(o[0], b'\\' | b'/')
        && matches!(o[1], b'\\' | b'/')
        && matches!(o[2], b'?' | b'.')
        && matches!(o[3], b'\\' | b'/')
    {
        return true;
    }
    if !crate::chemin_inaccessible::est_un_chemin_unc(c) {
        return false;
    }
    let segs = segments_lexicaux(c);
    let hote = segs
        .first()
        .map(|h| h.to_ascii_lowercase())
        .unwrap_or_default();
    let hote_local = hote == "localhost"
        || hote.starts_with("127.")
        || hote == "::1"
        || hote == "[::1]"
        || hote == "0:0:0:0:0:0:0:1"
        || hote == "0.0.0.0"
        || hote == "wsl.localhost"
        || hote == "wsl$"
        || hote.ends_with(".ipv6-literal.net");
    let partage_admin = segs.get(1).is_some_and(|p| p.ends_with('$'));
    hote_local || partage_admin
}

/// Un enfant DIRECT de `racine` est-il un lien qui mène hors du périmètre ?
/// (`/Volumes/Macintosh HD → /`, `~/.wine/dosdevices/z: → /`.) Le client n'a
/// pas besoin de créer le lien : il lui suffirait de choisir le parent.
/// `file_type` ne suit pas les liens : seuls les liens sont résolus.
fn un_enfant_mene_hors_perimetre(racine: &std::path::Path) -> bool {
    let Ok(lecture) = std::fs::read_dir(racine) else {
        return false;
    };
    for entree in lecture.flatten().take(ENFANTS_INSPECTES_MAX) {
        let est_un_lien = entree.file_type().is_ok_and(|t| t.is_symlink());
        if est_un_lien
            && let Some(cible) = forme_canonique(&entree.path())
            && cible_trop_large(&cible)
        {
            return true;
        }
    }
    false
}

/// Comme [`verifier_une_racine_de_musique`], avec les dossiers de montage
/// réseau de Tune (`montages`) : une racine posée DANS l'un d'eux reste
/// acceptée même s'il vit sous un arbre système (installation `.deb` :
/// `/var/lib/tune/montages/<partage>`). Toutes les autres règles
/// s'appliquent, et la forme canonique doit rester dans le montage.
pub(crate) fn verifier_une_racine_de_musique_avec(
    chemin: &str,
    montages: &[String],
) -> Result<(), Refus> {
    let c = chemin.trim();
    if !est_absolu(c) {
        return Err(Refus::PasAbsolu);
    }
    if remonte_vers_le_parent(c) || tune_core::chemin_sur::sans_remontee(c).is_err() {
        return Err(Refus::RemonteVersLeParent);
    }
    if est_un_chemin_windows_interdit(c) {
        return Err(Refus::TropLarge);
    }
    let canonique = forme_canonique(std::path::Path::new(c));
    let dans_un_montage = !montages.is_empty() && {
        let replie = forme_repliee(c);
        montages.iter().any(|m| {
            let m_replie = forme_repliee(m);
            let lexical = est_absolu(m)
                && !est_une_racine_de_disque(&m_replie)
                && tune_core::chemin_sur::sous_la_racine(&replie, &m_replie);
            let reel = match (&canonique, forme_canonique(std::path::Path::new(m))) {
                (Some(cc), Some(cm)) => tune_core::chemin_sur::sous_la_racine(cc, &cm),
                (Some(_), None) => false,
                (None, _) => true,
            };
            lexical && reel
        })
    };
    if !dans_un_montage && (dans_un_arbre_systeme(c) || dans_un_arbre_systeme(&forme_repliee(c))) {
        return Err(Refus::ArbreSysteme);
    }
    if est_une_racine_de_disque(c)
        || est_un_rassemblement_de_volumes(c)
        || (!dans_un_montage && contient_un_arbre_systeme(c))
    {
        return Err(Refus::TropLarge);
    }
    if let Some(canonique) = &canonique {
        if !dans_un_montage && dans_un_arbre_systeme(canonique) {
            return Err(Refus::ArbreSysteme);
        }
        if est_une_racine_de_disque(canonique)
            || est_un_rassemblement_de_volumes(canonique)
            || est_un_chemin_windows_interdit(canonique)
            || (!dans_un_montage && contient_un_arbre_systeme(canonique))
        {
            return Err(Refus::TropLarge);
        }
    }
    if un_enfant_mene_hors_perimetre(std::path::Path::new(c)) {
        return Err(Refus::TropLarge);
    }
    Ok(())
}

// ───────────────────────────────────────────────────────────────────────────
// Lecteurs Windows et garde-fou d'ajout (fil forum 2171)
// ───────────────────────────────────────────────────────────────────────────

/// La requête demande-t-elle la LISTE DES LECTEURS (Windows) ?
///
/// `/` et `\` n'ont pas de sens sous Windows en dehors d'un lecteur : ils
/// désignent « le poste », c'est-à-dire la liste `C:\`, `D:\`… Une racine de
/// lecteur (`C:\`), elle, désigne son CONTENU. La route confondait les deux :
/// `C:\` rendait la liste des lecteurs, si bien qu'un clic sur « C » ramenait
/// à la même liste et que le lecteur système n'était jamais explorable.
pub(crate) fn liste_des_lecteurs_demandee(chemin: &str) -> bool {
    matches!(chemin.trim(), "" | "/" | "\\")
}

/// `C:\`, `C:/` ou `C:` : la racine d'un lecteur Windows.
pub(crate) fn est_une_racine_de_lecteur(chemin: &str) -> bool {
    crate::chemin_inaccessible::lettre_de_lecteur(chemin).is_some()
        && chemin[2..].trim_matches(['/', '\\']).is_empty()
}

/// Le parent à rendre au client. Sous Windows, une racine de lecteur remonte
/// vers la liste des lecteurs (`/`), sans quoi le bouton « .. » disparaît et
/// l'on reste prisonnier du lecteur où l'on est entré.
pub(crate) fn parent_a_rendre(
    chemin: &str,
    parent_natif: Option<String>,
    windows: bool,
) -> Option<String> {
    if windows && est_une_racine_de_lecteur(chemin) {
        return Some("/".into());
    }
    parent_natif
}

/// Durée maximale du comptage qui précède l'ajout d'un dossier. Au-delà, le
/// compte est rendu tel quel, marqué incomplet : « au moins N fichiers ».
pub(crate) const ESTIMATION_DUREE_MAX: std::time::Duration = std::time::Duration::from_millis(1500);

/// Nombre maximal d'entrées visitées par ce comptage, quelle que soit la
/// vitesse du disque.
pub(crate) const ESTIMATION_ENTREES_MAX: u64 = 200_000;

/// Ce que l'ajout d'un dossier ferait analyser, mesuré vite et borné.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Estimation {
    pub fichiers_audio: u64,
    pub dossiers: u64,
    /// `false` : le comptage s'est arrêté sur une borne (durée ou entrées).
    /// Les nombres sont alors des MINIMUMS.
    pub complete: bool,
}

/// Compte les fichiers audio sous `racine`, sans suivre les liens
/// symboliques, sans entrer dans les dossiers cachés ni dans les arbres
/// système, et sans dépasser ni `echeance` ni `entrees_max`.
///
/// Les extensions sont celles du scan (`LIBRARY_AUDIO_EXTENSIONS`) : le
/// chiffre annoncé est celui que le scan trouverait.
pub(crate) fn estimer_le_contenu(
    racine: &std::path::Path,
    echeance: std::time::Instant,
    entrees_max: u64,
) -> Estimation {
    use tune_core::audio::support::LIBRARY_AUDIO_EXTENSIONS;

    let mut e = Estimation {
        complete: true,
        ..Estimation::default()
    };
    let mut entrees: u64 = 0;
    let mut a_visiter = vec![racine.to_path_buf()];
    while let Some(dossier) = a_visiter.pop() {
        let Ok(lecture) = std::fs::read_dir(&dossier) else {
            continue;
        };
        for entree in lecture.flatten() {
            entrees += 1;
            if entrees > entrees_max || std::time::Instant::now() >= echeance {
                e.complete = false;
                return e;
            }
            let nom = entree.file_name();
            let nom = nom.to_string_lossy();
            if nom.starts_with('.') {
                continue;
            }
            // `file_type` ne suit pas les liens : un lien n'est ni compté ni
            // parcouru, ce qui écarte aussi les boucles.
            let Ok(genre) = entree.file_type() else {
                continue;
            };
            if genre.is_dir() {
                let chemin = entree.path();
                // Fil 2207 — mêmes corbeilles que le scan, casse ignorée.
                if tune_core::scanner::dossier_systeme_ignore(&nom)
                    || dans_un_arbre_systeme(&chemin.to_string_lossy())
                {
                    continue;
                }
                e.dossiers += 1;
                a_visiter.push(chemin);
            } else if genre.is_file() {
                let audio = std::path::Path::new(&*nom)
                    .extension()
                    .map(|x| x.to_string_lossy().to_ascii_lowercase())
                    .is_some_and(|x| LIBRARY_AUDIO_EXTENSIONS.contains(&x.as_str()));
                if audio {
                    e.fichiers_audio += 1;
                }
            }
        }
    }
    e
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------
    // Les REFUS. Chacun tombe si l'on retire la garde correspondante :
    // c'est la contre-épreuve, et elle vaut plus que le chemin nominal.
    // -----------------------------------------------------------------

    /// Le `..` est refusé quel que soit le séparateur qui l'entoure. Le piège
    /// Windows a déjà mordu quatre fois sur ce dépôt (#2016) : une garde
    /// écrite avec le seul `/` laisse passer toute la forme antislash.
    #[test]
    fn un_retour_arriere_est_refuse_avec_les_deux_separateurs() {
        for chemin in [
            "/home/bertrand/../../etc",
            "/home/bertrand/..",
            "../etc/passwd",
            r"D:\Musique\..\..\Windows",
            r"D:\Musique\..",
            "D:/Musique/../Windows",
            r"\\NAS\Musique\..\..\etc",
        ] {
            assert!(
                remonte_vers_le_parent(chemin),
                "un « .. » non vu : {chemin:?}"
            );
        }
    }

    /// Contre-épreuve du séparateur : la formule naïve — celle que ce dépôt a
    /// écrite quatre fois — ne voit AUCUN des `..` de la forme Windows. Ce
    /// test échoue si quelqu'un revient à `split('/')`.
    #[test]
    fn l_ancien_decoupage_sur_le_seul_slash_ratait_la_forme_windows() {
        let naif = |chemin: &str| chemin.split('/').any(|s| s == "..");
        assert!(!naif(r"D:\Musique\..\..\Windows"));
        assert!(remonte_vers_le_parent(r"D:\Musique\..\..\Windows"));
    }

    /// Un chemin sain ne doit pas être pris pour une remontée : `..` est un
    /// SEGMENT, pas une sous-chaîne. Un dossier nommé « ..soundtracks »
    /// existe, et refuser l'accès à la bibliothèque serait une régression.
    #[test]
    fn un_nom_qui_commence_par_deux_points_n_est_pas_une_remontee() {
        assert!(!remonte_vers_le_parent("/mnt/musique/..soundtracks"));
        assert!(!remonte_vers_le_parent(r"D:\Musique\..bootlegs"));
    }

    /// Les arbres système sont refusés, dans les deux jeux de règles, depuis
    /// n'importe quel hôte.
    #[test]
    fn les_arbres_systeme_sont_refuses() {
        for chemin in [
            "/etc",
            "/etc/ssh",
            "/proc/1/environ",
            "/root",
            "/sys/class",
            "/var/lib",
            "/private/var/db",
            "/System/Library",
            r"C:\Windows",
            r"C:\Windows\System32\config",
            "C:/Windows/System32",
            r"c:\windows\system32",
            r"D:\Program Files (x86)",
            r"E:\$Recycle.Bin",
            r"C:\ProgramData\Tune",
        ] {
            assert!(dans_un_arbre_systeme(chemin), "laissé passer : {chemin:?}");
        }
    }

    /// Frontière de séparateur, cinquième occurrence de #2016 évitée : un
    /// dossier dont le NOM commence par celui d'un arbre système n'est pas
    /// dans cet arbre. `/etcetera` et `/vars` sont des dossiers ordinaires.
    #[test]
    fn un_prefixe_de_nom_n_est_pas_un_arbre_systeme() {
        for chemin in [
            "/etcetera/musique",
            "/vars",
            "/usrlocal/musique",
            "/systeme",
            r"C:\Windows Media",
            r"C:\ProgramData2",
        ] {
            assert!(
                !dans_un_arbre_systeme(chemin),
                "refusé à tort : {chemin:?} — préfixe de nom pris pour containment"
            );
        }
    }

    /// Ce que l'explorateur doit continuer à ouvrir. Sans ce test, resserrer
    /// le périmètre « pour être tranquille » casserait le geste que #1275
    /// demande, sans que rien ne le dise.
    #[test]
    fn les_endroits_ou_vit_la_musique_restent_ouverts() {
        for chemin in [
            "/",
            "/home/bertrand/Musique",
            "/Users/bertrand/Music",
            "/Volumes/Musique",
            "/mnt/nas/Jazz",
            "/media/bertrand/USB",
            "/run/media/bertrand/USB",
            "/srv/musique",
            "/opt/musique",
            "/data/music",
            "/nas/Blues 2",
            r"D:\Musique",
            r"C:\Users\bertrand\Music",
            r"\\NAS\Musique\Jazz",
            "//NAS/Musique/Jazz",
        ] {
            assert!(
                !dans_un_arbre_systeme(chemin),
                "fermé à tort : {chemin:?} — la musique vit là"
            );
        }
    }

    /// `Path::is_absolute()` aurait déclaré tous les chemins Windows relatifs
    /// sur l'hôte POSIX du CI : la garde aurait refusé Windows en entier tout
    /// en restant verte. Ce test fixe la reconnaissance des trois écritures.
    #[test]
    fn les_trois_ecritures_absolues_sont_reconnues_depuis_n_importe_quel_hote() {
        for chemin in [
            "/mnt/musique",
            r"D:\Musique",
            "D:/Musique",
            "Z:",
            r"\\NAS\Musique",
            "//NAS/Musique",
        ] {
            assert!(est_absolu(chemin), "déclaré relatif : {chemin:?}");
        }
        for chemin in ["", "Musique", "./Musique", "../Musique", "musique/jazz"] {
            assert!(!est_absolu(chemin), "déclaré absolu : {chemin:?}");
        }
        // Contre-épreuve de l'angle mort : sur un hôte POSIX, `is_absolute`
        // ment sur la forme Windows. C'est la raison d'être de `est_absolu`.
        #[cfg(not(target_os = "windows"))]
        assert!(!std::path::Path::new(r"D:\Musique").is_absolute());
    }

    /// La garde d'entrée refuse pour la BONNE raison — un message qui se
    /// trompe de cause envoie l'utilisateur chercher ailleurs.
    #[test]
    fn la_garde_d_entree_nomme_le_motif_du_refus() {
        assert_eq!(verifier_le_chemin_demande("Musique"), Err(Refus::PasAbsolu));
        assert_eq!(
            verifier_le_chemin_demande(r"D:\Musique\..\Windows"),
            Err(Refus::RemonteVersLeParent)
        );
        assert_eq!(
            verifier_le_chemin_demande("/etc/ssh"),
            Err(Refus::ArbreSysteme)
        );
        assert_eq!(verifier_le_chemin_demande("/mnt/musique"), Ok(()));
        assert_eq!(verifier_le_chemin_demande(r"D:\Musique"), Ok(()));
    }

    /// Le lien symbolique : le texte du chemin est irréprochable, la cible ne
    /// l'est pas. Sans la forme canonique, le refus ne se déclenche jamais.
    #[cfg(unix)]
    #[test]
    fn un_lien_qui_pointe_vers_un_arbre_systeme_est_refuse() {
        // `/tmp` et non `std::env::temp_dir()` : sous macOS ce dernier vit
        // sous `/private/var`, donc DÉJÀ hors périmètre, et le test passerait
        // pour la mauvaise raison. `/tmp` (`/private/tmp` après résolution)
        // est dans le périmètre sur les deux systèmes.
        let base = tune_core::test_scratch::scratch_dir_in("/tmp", "tune-explorateur-lien-i1275");
        let lien = base.join("sys");
        std::os::unix::fs::symlink("/etc", &lien).expect("lien de test");

        // Le texte passe la garde d'entrée : c'est bien le piège.
        assert_eq!(
            verifier_le_chemin_demande(&lien.to_string_lossy()),
            Ok(()),
            "le texte du lien devrait passer — sinon ce test ne prouve rien"
        );
        // La cible, elle, est refusée.
        assert!(!la_cible_reste_dans_le_perimetre(&lien));
        // Contre-épreuve : un dossier ordinaire du même endroit passe.
        let ordinaire = base.join("Musique");
        std::fs::create_dir_all(&ordinaire).expect("dossier de test");
        assert!(la_cible_reste_dans_le_perimetre(&ordinaire));
    }

    /// Le préfixe « verbatim » de Windows est retiré, sinon aucune règle ne
    /// reconnaîtrait `\\?\C:\Windows` comme `C:\Windows`.
    #[test]
    fn la_forme_verbatim_de_windows_est_ramenee_a_sa_forme_ordinaire() {
        // `forme_canonique` passe par le disque ; c'est le retrait du préfixe
        // qu'on vérifie ici, sur le même texte.
        let verbatim = r"\\?\C:\Windows\System32";
        let ordinaire = verbatim.strip_prefix(r"\\?\").unwrap();
        assert!(!dans_un_arbre_systeme(verbatim));
        assert!(dans_un_arbre_systeme(ordinaire));
    }

    // -----------------------------------------------------------------
    // Sécurité — périmètre d'une racine de bibliothèque.
    // -----------------------------------------------------------------

    #[test]
    fn une_racine_de_musique_n_est_ni_un_disque_ni_un_arbre_systeme() {
        for (chemin, motif) in [
            ("/", Refus::TropLarge),
            ("//", Refus::TropLarge),
            ("/./", Refus::TropLarge),
            (r"\", Refus::PasAbsolu),
            (r"C:\", Refus::TropLarge),
            ("c:", Refus::TropLarge),
            (r"D:\.\", Refus::TropLarge),
            (r"\\NAS", Refus::TropLarge),
            ("//NAS/", Refus::TropLarge),
            ("/private", Refus::TropLarge),
            ("/etc", Refus::ArbreSysteme),
            ("//etc", Refus::ArbreSysteme),
            ("/./etc/ssh", Refus::ArbreSysteme),
            ("/root/", Refus::ArbreSysteme),
            ("/proc/self", Refus::ArbreSysteme),
            (r"C:\Windows", Refus::ArbreSysteme),
            (r"C:\.\Windows\System32", Refus::ArbreSysteme),
            ("/musique/../etc", Refus::RemonteVersLeParent),
            ("/musique/%2e%2e/etc", Refus::RemonteVersLeParent),
            ("musique", Refus::PasAbsolu),
            ("", Refus::PasAbsolu),
        ] {
            assert_eq!(
                verifier_une_racine_de_musique(chemin),
                Err(motif),
                "{chemin:?}"
            );
        }
    }

    /// S1 — `/Volumes` rassemble les volumes, dont `Macintosh HD → /`.
    #[test]
    fn le_dossier_des_volumes_n_est_pas_une_racine_de_musique() {
        for c in [
            "/Volumes",
            "/Volumes/",
            "//Volumes",
            "/volumes",
            "/System/Volumes",
        ] {
            assert!(verifier_une_racine_de_musique(c).is_err(), "{c:?} accepté");
        }
        assert_eq!(
            verifier_une_racine_de_musique("/Volumes"),
            Err(Refus::TropLarge)
        );
        // Contre-épreuve : un volume de musique, lui, se déclare.
        assert_eq!(verifier_une_racine_de_musique("/Volumes/MUSIC_SSD"), Ok(()));
    }

    /// S1 — un dossier qui CONTIENT directement un lien vers `/` ou vers un
    /// arbre système (`~/.wine/dosdevices/z: → /`) n'est pas une racine.
    #[cfg(unix)]
    #[test]
    fn un_dossier_qui_contient_un_lien_vers_le_disque_n_est_pas_une_racine() {
        for (cible, etiquette) in [("/", "racine"), ("/etc", "etc")] {
            let base = tune_core::test_scratch::scratch_dir_in(
                "/tmp",
                &format!("tune-racine-enfant-{etiquette}"),
            );
            std::fs::create_dir_all(base.path().join("Album")).unwrap();
            std::os::unix::fs::symlink(cible, base.path().join("Macintosh HD")).unwrap();
            assert_eq!(
                verifier_une_racine_de_musique(&base.path().to_string_lossy()),
                Err(Refus::TropLarge),
                "lien vers {cible}"
            );
            // L'enfant ordinaire, lui, se déclare toujours.
            assert_eq!(
                verifier_une_racine_de_musique(&base.path().join("Album").to_string_lossy()),
                Ok(())
            );
        }
        // Contre-épreuve : un lien vers un autre dossier de musique est permis.
        let base = tune_core::test_scratch::scratch_dir_in("/tmp", "tune-racine-enfant-ok");
        let ailleurs = tune_core::test_scratch::scratch_dir_in("/tmp", "tune-racine-ailleurs");
        std::os::unix::fs::symlink(ailleurs.path(), base.path().join("Jazz")).unwrap();
        assert_eq!(
            verifier_une_racine_de_musique(&base.path().to_string_lossy()),
            Ok(())
        );
    }

    /// S4 — partages d'administration, partages de la machine elle-même et
    /// préfixes de périphérique Windows.
    #[test]
    fn les_partages_windows_qui_ouvrent_un_disque_sont_refuses() {
        for c in [
            r"\\localhost\C$",
            r"\\localhost\Musique",
            r"\\LOCALHOST\c$\Users",
            r"\\127.0.0.1\C$",
            r"\\127.0.0.1\Musique",
            r"\\[::1]\Musique",
            r"\\NAS\C$",
            r"\\NAS\ADMIN$",
            r"\\?\UNC\localhost\C$",
            r"\\?\UNC\NAS\Musique",
            r"\\?\C:\Musique",
            r"\\.\C:\Musique",
            "//?/C:/Musique",
            r"\\wsl.localhost\Ubuntu",
            r"\\wsl$\Ubuntu\home",
        ] {
            assert_eq!(
                verifier_une_racine_de_musique(c),
                Err(Refus::TropLarge),
                "{c:?}"
            );
        }
        for c in [
            r"\\NAS\Musique",
            r"\\192.168.1.10\Musique",
            "//NAS/Musique/Jazz",
        ] {
            assert_eq!(verifier_une_racine_de_musique(c), Ok(()), "{c:?}");
        }
    }

    /// Montages SMB des installations `.deb` : `/var/lib/tune/montages`
    /// reste une racine valide, rien d'autre sous `/var`.
    #[test]
    fn un_partage_monte_par_tune_reste_declarable_meme_sous_var() {
        let montages = vec!["/var/lib/tune/montages".to_string()];
        for c in [
            "/var/lib/tune/montages/nas-musique",
            "/var/lib/tune/montages/nas-musique/Jazz",
        ] {
            assert_eq!(
                verifier_une_racine_de_musique_avec(c, &montages),
                Ok(()),
                "{c:?}"
            );
            assert_eq!(
                verifier_une_racine_de_musique(c),
                Err(Refus::ArbreSysteme),
                "sans l'exception, {c:?} reste refusé"
            );
        }
        for c in [
            "/var/lib/tune",
            "/var/lib/tune/montages/../tune.db",
            "/var/lib/tune/montages2",
            "/var/log",
            "/etc",
        ] {
            assert!(
                verifier_une_racine_de_musique_avec(c, &montages).is_err(),
                "{c:?} accepté par l'exception des montages"
            );
        }
        // Une racine de montage relative ne vaut rien.
        assert!(verifier_une_racine_de_musique_avec("/var/x", &["var".to_string()]).is_err());
    }

    /// Contre-épreuve : ce que l'on déclare vraiment comme bibliothèque.
    #[test]
    fn une_vraie_bibliotheque_reste_declarable() {
        for chemin in [
            "/home/bertrand/Musique",
            "/Users/bertrand/Music",
            "/Volumes/Musique",
            "/mnt/nas/Jazz",
            "/media/bertrand/USB",
            "/run/media/bertrand/USB",
            "/srv/musique",
            "/nas/Blues 2",
            r"D:\Musique",
            r"\\NAS\Musique",
            "//NAS/Musique/Jazz",
            "  /srv/musique  ",
        ] {
            assert_eq!(verifier_une_racine_de_musique(chemin), Ok(()), "{chemin:?}");
        }
    }

    /// Un lien `~/Musique/tout → /` : le texte est sain, la cible est le
    /// disque entier.
    #[cfg(unix)]
    #[test]
    fn un_lien_vers_la_racine_n_est_pas_une_racine_de_musique() {
        let base = tune_core::test_scratch::scratch_dir_in("/tmp", "tune-racine-lien");
        let lien = base.join("tout");
        std::os::unix::fs::symlink("/", &lien).expect("lien de test");
        assert_eq!(
            verifier_une_racine_de_musique(&lien.to_string_lossy()),
            Err(Refus::TropLarge)
        );
        let lien_etc = base.join("conf");
        std::os::unix::fs::symlink("/etc", &lien_etc).expect("lien de test");
        assert_eq!(
            verifier_une_racine_de_musique(&lien_etc.to_string_lossy()),
            Err(Refus::ArbreSysteme)
        );
        let ordinaire = base.join("Musique");
        std::fs::create_dir_all(&ordinaire).unwrap();
        assert_eq!(
            verifier_une_racine_de_musique(&ordinaire.to_string_lossy()),
            Ok(())
        );
    }

    // -----------------------------------------------------------------
    // Lecteurs Windows et garde-fou d'ajout (fil forum 2171).
    // -----------------------------------------------------------------

    /// `C:\` désigne le CONTENU du lecteur, pas la liste des lecteurs. Ce
    /// test tombe si l'on revient à l'ancienne condition, où `C:\` rendait la
    /// liste et rendait le lecteur système inexplorable.
    #[test]
    fn une_racine_de_lecteur_n_est_pas_la_liste_des_lecteurs() {
        for chemin in ["/", "\\", ""] {
            assert!(liste_des_lecteurs_demandee(chemin), "{chemin:?}");
        }
        for chemin in [r"C:\", "C:/", r"D:\Musique", "/mnt/musique"] {
            assert!(!liste_des_lecteurs_demandee(chemin), "{chemin:?}");
        }
    }

    #[test]
    fn les_racines_de_lecteur_sont_reconnues() {
        for chemin in [r"C:\", "D:/", "e:", r"Z:\\"] {
            assert!(est_une_racine_de_lecteur(chemin), "{chemin:?}");
        }
        for chemin in [r"C:\Musique", "/", r"\\NAS\Musique", "C:toto"] {
            assert!(!est_une_racine_de_lecteur(chemin), "{chemin:?}");
        }
    }

    /// Sous Windows, « .. » depuis `D:\` ramène à la liste des lecteurs ;
    /// ailleurs, le parent natif est rendu tel quel.
    #[test]
    fn une_racine_de_lecteur_remonte_vers_la_liste_des_lecteurs() {
        assert_eq!(parent_a_rendre(r"D:\", None, true).as_deref(), Some("/"));
        assert_eq!(
            parent_a_rendre(r"D:\Musique", Some(r"D:\".into()), true).as_deref(),
            Some(r"D:\")
        );
        assert_eq!(parent_a_rendre("/", None, false), None);
        assert_eq!(
            parent_a_rendre("/mnt/a", Some("/mnt".into()), false).as_deref(),
            Some("/mnt")
        );
    }

    /// `/tmp` et non `std::env::temp_dir()` : sous macOS ce dernier vit sous
    /// `/private/var`, arbre système dont le comptage écarte les enfants.
    #[cfg(unix)]
    fn arbre_de_test(etiquette: &str) -> tune_core::test_scratch::ScratchDir {
        let base = tune_core::test_scratch::scratch_dir_in("/tmp", etiquette);
        let a = base.join("Artiste").join("Album");
        std::fs::create_dir_all(&a).unwrap();
        for f in ["01.flac", "02.FLAC", "03.mp3", "cover.jpg", "notes.txt"] {
            std::fs::write(a.join(f), b"x").unwrap();
        }
        let cache = base.join(".cache");
        std::fs::create_dir_all(&cache).unwrap();
        std::fs::write(cache.join("cache.flac"), b"x").unwrap();
        base
    }

    /// Le comptage ne retient que les extensions du scan, casse ignorée, et
    /// n'entre pas dans les dossiers cachés.
    #[cfg(unix)]
    #[test]
    fn l_estimation_compte_les_fichiers_audio_du_scan() {
        let base = arbre_de_test("tune-estimation-2171");
        let e = estimer_le_contenu(
            &base,
            std::time::Instant::now() + std::time::Duration::from_secs(30),
            ESTIMATION_ENTREES_MAX,
        );
        assert_eq!(
            e,
            Estimation {
                fichiers_audio: 3,
                dossiers: 2,
                complete: true
            }
        );
    }

    /// Fil forum 2207 — l'estimation annonce ce que le scan trouverait : les
    /// corbeilles de NAS (`#recycle`, `@eaDir`, `$Recycle.Bin`…, casse
    /// ignorée) n'y entrent pas plus que dans le scan.
    #[cfg(unix)]
    #[test]
    fn l_estimation_ignore_les_corbeilles_des_nas_2207() {
        let base = arbre_de_test("tune-estimation-corbeilles-2207");
        for corbeille in [
            "#Recycle",
            "@EADIR",
            "$Recycle.Bin",
            "system volume information",
        ] {
            let d = base.join("Artiste").join(corbeille);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("x.flac"), b"x").unwrap();
        }
        let e = estimer_le_contenu(
            &base,
            std::time::Instant::now() + std::time::Duration::from_secs(30),
            ESTIMATION_ENTREES_MAX,
        );
        assert_eq!(
            e,
            Estimation {
                fichiers_audio: 3,
                dossiers: 2,
                complete: true
            },
            "🔴 fil 2207 — l'estimation compte les corbeilles de NAS"
        );
    }

    /// La borne d'entrées arrête le comptage et le dit : un disque entier ne
    /// peut pas faire attendre la réponse.
    #[cfg(unix)]
    #[test]
    fn l_estimation_s_arrete_sur_sa_borne_et_le_dit() {
        let base = arbre_de_test("tune-estimation-borne-2171");
        let e = estimer_le_contenu(
            &base,
            std::time::Instant::now() + std::time::Duration::from_secs(30),
            2,
        );
        assert!(!e.complete);
        let e = estimer_le_contenu(&base, std::time::Instant::now(), ESTIMATION_ENTREES_MAX);
        assert!(
            !e.complete,
            "une échéance dépassée doit arrêter le comptage"
        );
    }
}
