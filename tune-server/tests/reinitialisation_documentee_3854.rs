//! #3854 — la procédure de réinitialisation dit-elle encore la vérité ?
//!
//! Fuccaro, fil 1755 : « Comment faire pour repartir sur une installation
//! propre ? ». Rien ne le documentait : `git grep -i "LOCALAPPDATA\|TuneServer"
//! -- '*.md'` ne rendait AUCUN résultat le 11/09/2026, et le `README.md` ne cite
//! que `TUNE_DB_PATH` sans dire où ce chemin relatif atterrit.
//!
//! `docs/REINITIALISER-TUNE.md` comble ce trou. Mais une documentation de
//! CHEMINS est le pire endroit où laisser une dérive s'installer : elle envoie
//! l'utilisateur effacer un répertoire. Un document qui dit
//! `%LOCALAPPDATA%\TuneServer` alors que le code a déménagé ne se contente pas
//! d'être faux, il fait perdre des données — et il ne rougit nulle part.
//!
//! Ces gardes relisent donc le document ET le code, et exigent qu'ils racontent
//! la même histoire. Elles ne vérifient pas la prose : elles vérifient que
//! chaque chemin cité existe encore à l'endroit qui le produit.
//!
//! ⚠️ Ce fichier est un MODULE de `server_contracts.rs`. `autotests = false`
//!    dans `tune-server/Cargo.toml` : sans cette inscription, il ne serait
//!    jamais compilé — et cette garde serait verte contre rien.

use std::fs;
use std::path::{Path, PathBuf};

fn racine() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..")
}

fn lire(chemin: &str) -> String {
    fs::read_to_string(racine().join(chemin)).unwrap_or_else(|e| panic!("{chemin} illisible : {e}"))
}

const DOC: &str = "docs/REINITIALISER-TUNE.md";

/// Chaque chemin de données cité par le document doit encore être produit par
/// le code qui le produit.
///
/// La liste est un INVENTAIRE, pas un échantillon : y ajouter un chemin au
/// document sans l'ajouter ici, c'est le laisser dériver.
#[test]
fn les_chemins_de_donnees_documentes_sont_ceux_du_code() {
    let doc = lire(DOC);

    // (cité dans le document, fichier qui le produit, fragment à y retrouver)
    let attaches: [(&str, &str, &str); 6] = [
        // `format!("{d}\\TuneServer")` sur la branche `target_os = "windows"`.
        (
            r"%LOCALAPPDATA%\TuneServer",
            "tune-server/src/config.rs",
            r#"format!("{d}\\TuneServer")"#,
        ),
        // `paths.insert(0, format!("{appdata}\\Tune\\tune.toml"))`.
        (
            r"%APPDATA%\Tune\tune.toml",
            "tune-server/src/config.rs",
            r#"\\Tune\\tune.toml"#,
        ),
        // `MACOS_DATA_SUBDIR`.
        (
            "~/Library/Application Support/Tune/",
            "tune-server/src/config.rs",
            "Library/Application Support/Tune",
        ),
        (
            "~/Library/Logs/tune-server.log",
            "tune-server/src/config.rs",
            "Library/Logs",
        ),
        // Le service pose le répertoire de travail ; le `postrm` le conserve.
        (
            "/var/lib/tune/",
            "packaging/deb/tune-server.service",
            "WorkingDirectory=/var/lib/tune",
        ),
        // Les sauvegardes vivent à côté du fichier de base.
        (
            "`backups/`",
            "tune-core/src/db_backup.rs",
            r#"join("backups")"#,
        ),
    ];

    for (cite, fichier, fragment) in attaches {
        assert!(
            doc.contains(cite),
            "{DOC} ne cite plus `{cite}`.\n\
             Ce document est la seule réponse écrite à « comment repartir de zéro ». \
             Retirer un chemin de la liste, c'est laisser l'utilisateur chercher."
        );
        let source = lire(fichier);
        assert!(
            source.contains(fragment),
            "{DOC} envoie l'utilisateur vers `{cite}`, mais `{fichier}` ne contient \
             plus `{fragment}` : le code a déménagé et la documentation est restée.\n\
             Une documentation de chemins qui dérive ne se contente pas d'être fausse — \
             elle fait effacer le mauvais répertoire.\n\
             Corriger {DOC} ET cette liste, pas seulement l'un des deux."
        );
    }
}

/// Le document affirme que le désinstalleur Windows LAISSE les données en place.
///
/// C'est le cœur du signalement du fil 1755 : désinstaller puis réinstaller ne
/// donne pas une installation propre, et le testeur en conclut que réinitialiser
/// est impossible. Le jour où le désinstalleur apprendra à effacer les données —
/// c'est l'arbitrage ouvert de #3854 — ce test rougira, et c'est le but : la
/// procédure manuelle devra être réécrite dans la foulée, pas six mois plus tard.
#[test]
fn le_desinstalleur_windows_laisse_toujours_les_donnees_comme_le_dit_la_procedure() {
    let release = lire(".github/workflows/release.yml");
    let debut = release
        .find(r#"Section "Uninstall""#)
        .expect("la section `Uninstall` de l'installeur NSIS a disparu de release.yml");
    let reste = &release[debut..];
    let fin = reste
        .find("SectionEnd")
        .expect("la section `Uninstall` n'est pas refermée : l'analyse est cassée");
    let section = &reste[..fin];

    for donnees in ["LOCALAPPDATA\\\\TuneServer", "APPDATA\\\\Tune"] {
        assert!(
            !section.contains(donnees),
            "le désinstalleur Windows efface maintenant `{donnees}` :\n{section}\n\
             C'est une bonne nouvelle, et {DOC} est désormais FAUX — sa section 4 \
             affirme le contraire, et sa procédure manuelle envoie supprimer un \
             répertoire déjà supprimé. Mettre le document à jour, puis ce test."
        );
    }

    let doc = lire(DOC);
    assert!(
        doc.contains("désinstaller ne donne PAS une installation propre"),
        "{DOC} ne porte plus l'avertissement sur le désinstalleur Windows, alors que \
         le désinstalleur laisse toujours les données en place. C'est précisément ce \
         que le fil 1755 signalait."
    );
}

/// Le seul geste de remise à zéro offert à l'utilisateur n'efface que les
/// pistes — et le document doit continuer à le dire, tant que c'est vrai.
///
/// `TrackRepo::delete_all()` ne touche ni `settings`, ni les comptes, ni les
/// listes de lecture, ni les collections. Laisser croire l'inverse enverrait un
/// testeur « repartir de zéro » avec ses vieux réglages intacts, puis rouvrir un
/// fil pour dire que ça n'a pas marché.
#[test]
fn vider_la_bibliotheque_n_efface_toujours_que_les_pistes() {
    let repo = lire("tune-core/src/db/track_repo.rs");
    // ⚠️ La signature COMPLÈTE, pas `pub fn delete_all(`. Le même fichier porte,
    //    240 lignes plus haut, un `pub fn delete_all() -> &'static str` du module
    //    `sql` qui ne fait que rendre une chaîne. Première version de cette garde :
    //    elle lisait ce petit homonyme, n'y trouvait évidemment aucune table, et
    //    restait VERTE pendant qu'on ajoutait `DELETE FROM settings` dans la vraie
    //    fonction. Mesuré par sa propre contre-épreuve, le 11/09/2026.
    let debut = repo
        .find("pub fn delete_all(&self) -> Result<u64, TuneError> {")
        .expect(
            "`TrackRepo::delete_all(&self) -> Result<u64, TuneError>` a disparu ou a change \
             de signature : `POST /system/library/clear` ne vide plus ce que ce test croit",
        );
    let reste = &repo[debut..];
    let fin = reste
        .find("\n    }\n")
        .expect("`delete_all` n'est pas refermée : l'analyse est cassée");
    let corps = &reste[..fin];
    // Un garde qui ne trouve rien doit ÉCHOUER, pas passer à vide. Si le corps
    // extrait ne contient plus les DELETE qu'il est censé surveiller, c'est
    // l'extraction qui est cassée — et le vert ne couvrirait rien.
    assert!(
        corps.contains("DELETE FROM albums"),
        "le corps extrait de `delete_all` ne contient aucun `DELETE FROM albums` : \
         l'extraction vise la mauvaise fonction et ce test ne garde plus rien.\n{corps}"
    );

    for table in ["settings", "users", "playlists", "collections"] {
        assert!(
            !corps.contains(table),
            "`TrackRepo::delete_all` touche maintenant `{table}` :\n{corps}\n\
             {DOC} affirme que « Vider la bibliothèque » n'efface que les pistes. \
             Si ce n'est plus vrai, le document ment sur une opération DESTRUCTIVE. \
             Mettre le document à jour avec ce test."
        );
    }
    let doc = lire(DOC);
    assert!(
        doc.contains("**Rien d'autre.**"),
        "{DOC} ne dit plus que « Vider la bibliothèque » n'efface rien d'autre que \
         les pistes. C'est la seule phrase qui empêche un testeur de croire que ce \
         bouton le ramène à une installation neuve."
    );
}
