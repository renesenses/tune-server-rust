//! Les deux portées de #2939, mises d'accord.
//!
//! ## Le défaut
//!
//! Deux questions se posaient sur la même donnée, et se répondaient sur des
//! périmètres différents :
//!
//! | Question | Qui répondait | Sur quoi |
//! |---|---|---|
//! | « Cette piste est-elle déjà connue ? » | `TrackRepo::get_all_local_file_info` | `WHERE source = 'local'` |
//! | « Ai-je le droit d'écrire cette ligne ? » | `file_path TEXT UNIQUE` | la table ENTIÈRE |
//!
//! Une ligne dont le `file_path` existait déjà sous une source autre que
//! `local` était donc **invisible à la première** et **très visible à la
//! seconde** : le scan la routait vers l'INSERTION, et la base la refusait.
//!
//! Chez Alain Bonnel (fil forum 1313, journal du 08/08/2026, partage Windows
//! UNC, 58 360 fichiers), c'est un album entier — les quatorze pistes de
//! *As We Once Were* — qui a disparu de cette façon, en trois secondes :
//!
//! ```text
//! walker:     batched_scan_complete total=14 metadata_ok=14 metadata_failed=0
//! track_repo: track_insert_failed_in_batch … UNIQUE constraint failed: tracks.file_path   (×14)
//! ```
//!
//! ## Pourquoi la carte, et pas la contrainte
//!
//! Les deux corrections étaient possibles — élargir la carte, ou rendre la
//! contrainte partielle (`UNIQUE (file_path) WHERE source = 'local'`). C'est la
//! première qui est juste, et le code le dit :
//!
//! - un flux Qobuz/Tidal **n'a jamais** de `file_path`
//!   (`orchestrator.rs` : « Média streaming : PAS de file_path ») ;
//! - une piste mise en cache hors ligne vit dans une table à elle,
//!   `offline_cache`, avec sa propre colonne `file_path` — jamais dans
//!   `tracks` ;
//! - les seules lignes de `tracks` non locales qui portent un `file_path` sont
//!   celles des importateurs de bibliothèque (`roon_import`, `plex_import`,
//!   `jriver`, dans `routes/system/import.rs`), et elles décrivent **le fichier
//!   que le scanner va rencontrer**, pas un autre.
//!
//! Deux lignes au même `file_path` sont donc la même piste. La contrainte a
//! raison, la carte avait tort. Aucune migration, rien d'irréversible.
//!
//! ## Ce que ce fichier tient
//!
//! 1. **La perte, reproduite** : la ligne d'importation existe, et l'ancienne
//!    route (insertion) se fait refuser par la contrainte — la piste est
//!    perdue.
//! 2. **Le correctif** : la MÊME situation passe par
//!    [`verdict_ecriture`], qui rend `MettreAJour`, et la piste entre.
//! 3. **L'adoption** : la ligne reprise devient `source = 'local'`, sinon le
//!    désaccord se rejouerait au scan suivant.
//! 4. **Le témoin** : un chemin inconnu part toujours en insertion, un chemin
//!    inchangé reste inchangé. Un verdict qui rendrait `MettreAJour` pour tout
//!    le monde passerait les trois premiers essais et tomberait ici.
//!
//! Tout passe par les fonctions de PRODUCTION —
//! [`tune_server::routes::system::scan::verdict_ecriture`],
//! [`TrackRepo::get_all_file_info_by_path`], [`TrackRepo::create_batch`],
//! [`TrackRepo::update_batch`], [`TrackRepo::adopter_en_local`] — jamais par
//! une transcription de leur logique.
//!
//! `autotests = false` dans `tune-server/Cargo.toml` : la cible `[[test]]` est
//! déclarée là-bas, sans quoi ce fichier ne serait jamais compilé.
use tune_core::db::models::Track;
use tune_core::db::sqlite::SqliteDb;
use tune_core::db::track_repo::TrackRepo;
use tune_server::routes::system::scan::{CarteDesChemins, VerdictEcriture, verdict_ecriture};

/// L'album d'Alain : quatorze pistes.
const PISTES_DE_L_ALBUM: usize = 14;
/// Ce que le parcours a lu sur le disque pour chacune.
const MTIME_DISQUE: u64 = 1_754_635_989;
const TAILLE_DISQUE: u64 = 41_236_112;

/// Une base SQLite SUR DISQUE, écrite puis relue : le schéma éprouvé est celui
/// qui a été posé sur le disque — contrainte d'unicité comprise — et non un
/// état de mémoire jamais relu.
fn base_sur_disque(dossier: &std::path::Path) -> SqliteDb {
    let url = dossier.join("tune.db").to_string_lossy().into_owned();
    {
        let db = SqliteDb::open(&url).expect("ouverture de la base de travail");
        db.init_schema().expect("schéma initialisé");
    }
    SqliteDb::open(&url).expect("réouverture de la base fermée")
}

/// Les quatorze chemins du partage UNC d'Alain, à l'octet près sur la forme.
fn chemins_de_l_album() -> Vec<String> {
    (1..=PISTES_DE_L_ALBUM)
        .map(|n| {
            format!(
                "\\\\192.168.0.2\\music\\The Anchoress\\As We Once Were\\{n:02} - Piste {n}.flac"
            )
        })
        .collect()
}

/// Pose les quatorze lignes telles qu'un importateur de bibliothèque les
/// laisse : un vrai `file_path`, une `source` qui n'est pas `local`, et aucune
/// empreinte de fichier (l'importateur ne lit pas le disque).
fn poser_les_lignes_importees(repo: &TrackRepo, chemins: &[String]) {
    let importees: Vec<Track> = chemins
        .iter()
        .enumerate()
        .map(|(i, chemin)| {
            let mut piste = Track::new(format!("Importée {i}"));
            piste.file_path = Some(chemin.clone());
            piste.source = "roon_import".into();
            piste
        })
        .collect();
    assert_eq!(
        repo.create_batch(&importees).unwrap(),
        PISTES_DE_L_ALBUM,
        "sans ces quatorze lignes la contrainte n'aurait rien à refuser et le \
         test ne mesurerait rien"
    );
}

/// La piste que le scanner construit à partir des balises du fichier.
fn piste_lue_sur_le_disque(chemin: &str, titre: &str) -> Track {
    let mut piste = Track::new(titre.to_string());
    piste.file_path = Some(chemin.to_string());
    piste.file_mtime = Some(MTIME_DISQUE as f64);
    piste.file_size = Some(TAILLE_DISQUE as i64);
    piste
}

fn carte_des_chemins(repo: &TrackRepo) -> CarteDesChemins {
    repo.get_all_file_info_by_path()
        .expect("la carte des chemins doit se charger")
}

#[test]
fn l_ancienne_route_perdait_l_album_entier() {
    // La contre-épreuve : ce que faisait le scan AVANT, et qui perdait tout.
    // Sans elle, rien ne prouve que la situation reproduite ici est bien celle
    // qui coûtait quatorze pistes à Alain.
    let base = tempfile::TempDir::new().unwrap();
    let repo = TrackRepo::new(base_sur_disque(base.path()));
    let chemins = chemins_de_l_album();
    poser_les_lignes_importees(&repo, &chemins);

    let a_inserer: Vec<Track> = chemins
        .iter()
        .map(|c| piste_lue_sur_le_disque(c, "Du Temps Perdu"))
        .collect();
    assert_eq!(
        repo.create_batch(&a_inserer).unwrap(),
        0,
        "la contrainte `file_path TEXT UNIQUE` porte sur la table ENTIÈRE : \
         les quatorze insertions sont refusées, et l'album est perdu"
    );
}

#[test]
fn une_ligne_importee_au_meme_chemin_part_en_mise_a_jour() {
    let base = tempfile::TempDir::new().unwrap();
    let repo = TrackRepo::new(base_sur_disque(base.path()));
    let chemins = chemins_de_l_album();
    poser_les_lignes_importees(&repo, &chemins);

    // 1. La carte voit ce que la contrainte voit. C'est TOUT le correctif :
    //    une portée pour deux questions qui décidaient de la même écriture.
    let carte = carte_des_chemins(&repo);
    assert_eq!(
        chemins.iter().filter(|c| carte.contains_key(*c)).count(),
        PISTES_DE_L_ALBUM,
        "chargée avec `WHERE source = 'local'`, la carte ne voyait AUCUNE des \
         quatorze lignes que la contrainte, elle, refusait très bien"
    );

    // 2. Le verdict de production, fichier par fichier.
    let mut a_mettre_a_jour: Vec<Track> = Vec::new();
    let mut a_adopter: Vec<i64> = Vec::new();
    for chemin in &chemins {
        match verdict_ecriture(chemin, MTIME_DISQUE, TAILLE_DISQUE, false, &carte) {
            VerdictEcriture::MettreAJour { id, adopter } => {
                assert!(
                    adopter,
                    "la ligne n'était pas locale : le scan vient de relire son \
                     fichier sur le disque, il doit la reprendre"
                );
                a_adopter.push(id);
                let mut piste = piste_lue_sur_le_disque(chemin, "Du Temps Perdu");
                piste.id = Some(id);
                a_mettre_a_jour.push(piste);
            }
            autre => panic!(
                "c'est ce verdict-là qui perdait l'album d'Alain : {autre:?} au \
                 lieu de MettreAJour, pour un chemin que la base possède déjà"
            ),
        }
    }

    // 3. L'écriture passe — aucune insertion, donc aucun refus possible.
    assert_eq!(
        repo.update_batch(&a_mettre_a_jour).unwrap(),
        PISTES_DE_L_ALBUM,
        "quatorze mises à jour, zéro insertion : plus rien à refuser"
    );
    assert_eq!(
        repo.adopter_en_local(&a_adopter).unwrap(),
        PISTES_DE_L_ALBUM,
        "les quatorze lignes reprises deviennent locales"
    );

    // 4. La bibliothèque tient bien quatorze pistes, une par fichier, et pas
    //    vingt-huit : la mise à jour a écrit DANS les lignes existantes.
    let carte = carte_des_chemins(&repo);
    assert_eq!(carte.len(), PISTES_DE_L_ALBUM, "une ligne par fichier");
    for chemin in &chemins {
        let info = carte.get(chemin).expect("le chemin est toujours là");
        assert!(
            info.est_locale(),
            "après reprise, la ligne appartient au scan : c'est ce qui empêche \
             le désaccord de se rejouer au scan suivant"
        );
        assert_eq!(
            info.taille,
            Some(TAILLE_DISQUE as i64),
            "la ligne porte désormais ce que le scan a lu sur le disque"
        );
    }
    let piste = repo
        .get_by_path(&chemins[0])
        .unwrap()
        .expect("la piste existe");
    assert_eq!(
        piste.title, "Du Temps Perdu",
        "c'est bien la lecture du disque qui a gagné, pas la ligne importée"
    );

    // 5. Un second scan trouve maintenant tout inchangé : l'état a convergé.
    let carte = carte_des_chemins(&repo);
    for chemin in &chemins {
        assert_eq!(
            verdict_ecriture(chemin, MTIME_DISQUE, TAILLE_DISQUE, false, &carte),
            VerdictEcriture::Inchange,
            "rien n'a bougé sur le disque : le scan suivant ne doit plus rien \
             réécrire"
        );
    }
}

#[test]
fn temoin_un_scan_sans_conflit_ne_change_pas_de_comportement() {
    // Sans ce témoin, un verdict qui rendrait `MettreAJour` pour n'importe quoi
    // passerait les essais ci-dessus pour un succès.
    let base = tempfile::TempDir::new().unwrap();
    let repo = TrackRepo::new(base_sur_disque(base.path()));
    let chemins = chemins_de_l_album();

    // Base vide : personne ne possède ces chemins.
    let carte = carte_des_chemins(&repo);
    assert!(carte.is_empty());
    for chemin in &chemins {
        assert_eq!(
            verdict_ecriture(chemin, MTIME_DISQUE, TAILLE_DISQUE, false, &carte),
            VerdictEcriture::Inserer,
            "un fichier que la base ne connaît pas s'insère, comme avant"
        );
    }
    let a_inserer: Vec<Track> = chemins
        .iter()
        .map(|c| piste_lue_sur_le_disque(c, "Du Temps Perdu"))
        .collect();
    assert_eq!(
        repo.create_batch(&a_inserer).unwrap(),
        PISTES_DE_L_ALBUM,
        "un scan nominal n'a aucun refus"
    );

    // Deuxième passage sur des fichiers qui n'ont pas bougé : rien à faire.
    let carte = carte_des_chemins(&repo);
    for chemin in &chemins {
        assert_eq!(
            verdict_ecriture(chemin, MTIME_DISQUE, TAILLE_DISQUE, false, &carte),
            VerdictEcriture::Inchange
        );
    }

    // Un fichier RETOUCHÉ sur le disque repart en mise à jour — sans adoption,
    // puisque la ligne était déjà locale. Le drapeau `adopter` ne doit pas être
    // un « vrai » constant : il commanderait alors une écriture inutile sur
    // toute la bibliothèque à chaque scan.
    match verdict_ecriture(&chemins[0], MTIME_DISQUE + 60, TAILLE_DISQUE, false, &carte) {
        VerdictEcriture::MettreAJour { adopter, .. } => assert!(
            !adopter,
            "une ligne déjà locale n'a personne à qui être reprise"
        ),
        autre => panic!("un fichier modifié se met à jour, pas {autre:?}"),
    }

    // `force` traverse le raccourci « inchangé » : c'est ce que fait le bouton
    // « re-scanner en profondeur », et ça ne doit pas changer.
    assert!(matches!(
        verdict_ecriture(&chemins[0], MTIME_DISQUE, TAILLE_DISQUE, true, &carte),
        VerdictEcriture::MettreAJour { .. }
    ));

    // Et l'adoption ne touche pas une ligne déjà locale : le compte rendu est
    // le nombre d'adoptions RÉELLES, pas le nombre d'identifiants présentés.
    let ids: Vec<i64> = carte.values().map(|i| i.id).collect();
    assert_eq!(
        repo.adopter_en_local(&ids).unwrap(),
        0,
        "quatorze lignes déjà locales, zéro adoption"
    );
}
