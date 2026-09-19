use std::fs;
use tune_core::sendspin::Identite;
use tune_core::sendspin::magasin::{ErreurMagasin, MagasinAppairage, MethodeAppairage};
use tune_core::sendspin::psk::{self, CategoriePsk, PskPair};

fn cle(client: &Identite, octet: u8) -> PskPair {
    PskPair::pour_pair(&client.id(), [octet; 32], CategoriePsk::LongueDuree).unwrap()
}

#[test]
fn i3326_identite_et_pairs_survivent_au_redemarrage_du_magasin() {
    let temporaire = tempfile::tempdir().unwrap();
    let dossier = temporaire.path().join("sendspin");
    let client = Identite::generer();
    let autre = Identite::generer();
    let mut magasin = MagasinAppairage::ouvrir(&dossier).unwrap();
    let server_id = magasin.identite().id();
    magasin
        .conserver(&client.id(), &cle(&client, 29), MethodeAppairage::Psk)
        .unwrap();
    magasin
        .conserver(
            &autre.id(),
            &cle(&autre, 31),
            MethodeAppairage::CodeDynamique,
        )
        .unwrap();
    drop(magasin);
    let mut relu = MagasinAppairage::ouvrir(&dossier).expect("relecture du magasin persiste");
    assert_eq!(
        relu.identite().id(),
        server_id,
        "l'identite serveur doit survivre au redemarrage"
    );
    assert_eq!(
        relu.cle_du_pair(&client.id())
            .unwrap()
            .expect("le pair doit survivre au redemarrage")
            .secret(),
        &[29; 32]
    );
    relu.conserver(
        &client.id(),
        &cle(&client, 47),
        MethodeAppairage::CodeStatique,
    )
    .unwrap();
    assert!(relu.retirer(&autre.id()).unwrap());
    assert!(!relu.retirer(&autre.id()).unwrap());
    drop(relu);
    let relu = MagasinAppairage::ouvrir(&dossier).unwrap();
    assert_eq!(relu.identite().id(), server_id);
    assert_eq!(
        relu.cle_du_pair(&client.id()).unwrap().unwrap().secret(),
        &[47; 32],
        "reappairer remplace la cle"
    );
    assert!(
        relu.cle_du_pair(&autre.id()).unwrap().is_none(),
        "la revocation doit persister"
    );
}

#[test]
fn i3326_le_magasin_refuse_la_sentinelle_et_les_cles_d_autres_clients() {
    let t = tempfile::tempdir().unwrap();
    let mut magasin = MagasinAppairage::ouvrir(&t.path().join("sendspin")).unwrap();
    let client = Identite::generer();
    let autre = Identite::generer();
    assert!(
        magasin
            .conserver(&client.id(), &PskPair::sentinelle(), MethodeAppairage::Psk)
            .is_err()
    );
    let provisoire = PskPair::pour_pair(&client.id(), [29; 32], CategoriePsk::Appairage).unwrap();
    assert!(
        magasin
            .conserver(&client.id(), &provisoire, MethodeAppairage::Psk)
            .is_err()
    );
    assert!(
        magasin
            .conserver(&client.id(), &cle(&autre, 29), MethodeAppairage::Psk)
            .is_err()
    );
    assert!(
        magasin.lister().unwrap().is_empty(),
        "un refus ne cree aucun appairage"
    );
}

#[test]
fn i3326_les_vues_du_magasin_ne_revelent_pas_les_secrets() {
    let t = tempfile::tempdir().unwrap();
    let mut magasin = MagasinAppairage::ouvrir(&t.path().join("sendspin")).unwrap();
    let client = Identite::generer();
    let psk = cle(&client, 29);
    magasin
        .conserver(&client.id(), &psk, MethodeAppairage::Psk)
        .unwrap();
    magasin
        .conserver(&client.id(), &psk, MethodeAppairage::CodeDynamique)
        .unwrap();
    let pairs = magasin.lister().unwrap();
    assert_eq!(
        pairs[0].methodes.len(),
        2,
        "la meme cle conserve ses methodes verifiees"
    );
    let texte = format!(
        "{magasin:?} {pairs:?} {}",
        serde_json::to_string(&pairs).unwrap()
    );
    for secret in [psk.secret(), magasin.identite().prive()] {
        assert!(!texte.contains(&tune_core::sendspin::identite::b64url(secret)));
        assert!(!texte.contains(&serde_json::to_string(secret).unwrap()));
        assert!(!texte.contains(&format!("{secret:?}")));
    }
}

#[test]
fn i3326_un_magasin_perdu_ou_corrompu_ne_regenere_jamais_l_identite() {
    let t = tempfile::tempdir().unwrap();
    let dossier = t.path().join("sendspin");
    drop(MagasinAppairage::ouvrir(&dossier).unwrap());
    let fichier = dossier.join("pairing.json");
    let original = fs::read(&fichier).unwrap();
    for corrompu in [b"".as_slice(), b"{", b"\"secret-ne-pas-journaliser\""] {
        fs::write(&fichier, corrompu).unwrap();
        let e = MagasinAppairage::ouvrir(&dossier).expect_err("document corrompu : refus");
        assert!(!format!("{e:?} {e}").contains("secret-ne-pas-journaliser"));
        assert_eq!(
            fs::read(&fichier).unwrap(),
            corrompu,
            "un echec ne remplace pas le fichier"
        );
    }
    let mut document: serde_json::Value = serde_json::from_slice(&original).unwrap();
    document["version"] = 999.into();
    fs::write(&fichier, document.to_string()).unwrap();
    assert!(
        MagasinAppairage::ouvrir(&dossier).is_err(),
        "pas de repli sur une version inconnue"
    );
    fs::remove_file(&fichier).unwrap();
    assert!(
        MagasinAppairage::ouvrir(&dossier).is_err(),
        "un fichier perdu ne signifie pas premier demarrage"
    );
    assert!(!fichier.exists());
}

#[test]
fn i3326_un_echec_d_ecriture_ne_publie_ni_cle_ni_succes_et_impose_un_rechargement() {
    let t = tempfile::tempdir().unwrap();
    let dossier = t.path().join("sendspin");
    let mut magasin = MagasinAppairage::ouvrir(&dossier).unwrap();
    let client = Identite::generer();
    magasin
        .conserver(&client.id(), &cle(&client, 29), MethodeAppairage::Psk)
        .unwrap();
    let fichier = dossier.join("pairing.json");
    let sauvegarde = dossier.join("avant-panne.json");
    fs::rename(&fichier, &sauvegarde).unwrap();
    fs::create_dir(&fichier).unwrap(); // destination invalide, sans chmod dependant du compte CI
    assert!(
        magasin
            .conserver(&client.id(), &cle(&client, 47), MethodeAppairage::Psk)
            .is_err()
    );
    assert!(matches!(
        magasin.cle_du_pair(&client.id()),
        Err(ErreurMagasin::Indisponible)
    ));
    assert!(matches!(
        magasin.retirer(&client.id()),
        Err(ErreurMagasin::Indisponible)
    ));
    fs::remove_dir(&fichier).unwrap();
    fs::rename(&sauvegarde, &fichier).unwrap();
    drop(magasin);
    let relu = MagasinAppairage::ouvrir(&dossier).unwrap();
    assert_eq!(
        relu.cle_du_pair(&client.id()).unwrap().unwrap().secret(),
        &[29; 32],
        "une sauvegarde ratee ne doit pas annoncer la nouvelle cle"
    );
    assert!(
        !fs::read_dir(&dossier).unwrap().any(|e| e
            .unwrap()
            .file_name()
            .to_string_lossy()
            .ends_with(".tmp"))
    );
}

#[test]
fn i3326_un_temporaire_abandonne_n_est_pas_un_magasin_de_secours() {
    let t = tempfile::tempdir().unwrap();
    let dossier = t.path().join("sendspin");
    let magasin = MagasinAppairage::ouvrir(&dossier).unwrap();
    let id = magasin.identite().id();
    drop(magasin);
    fs::write(dossier.join(".pairing-abandon.tmp"), b"{").unwrap();
    assert_eq!(
        MagasinAppairage::ouvrir(&dossier).unwrap().identite().id(),
        id
    );
}

/// Le verrou `flock` appartient a la *description de fichier ouverte*, pas au
/// descripteur : un `fork()` concurrent en duplique une copie que la fermeture
/// du notre n'annule pas. C'est exactement ce que laisse, le temps d'un
/// `exec()`, tout `Command::spawn` lance par un fil voisin -- le temoin
/// multiprocessus juste au-dessus, entre autres. Sans relachement explicite du
/// verrou, rouvrir le magasin aussitot apres un `drop` rend `Occupe` sans
/// qu'aucun autre magasin ne soit ouvert : le faux rouge de #4331, visible sur
/// les runners GitHub ou la fenetre fork->exec dure, et invisible ailleurs.
#[cfg(unix)]
#[test]
fn i4331_un_fork_concurrent_ne_retient_pas_le_verrou_apres_le_drop() {
    let t = tempfile::tempdir().unwrap();
    let dossier = t.path().join("sendspin");
    let magasin = MagasinAppairage::ouvrir(&dossier).unwrap();
    let id = magasin.identite().id();
    // L'enfant se contente d'attendre : apres un fork depuis un processus
    // multi-fils, rien d'autre n'est sur. Il detient une copie du descripteur.
    let enfant = unsafe { libc::fork() };
    assert!(enfant >= 0, "fork impossible");
    if enfant == 0 {
        unsafe {
            libc::sleep(30);
            libc::_exit(0);
        }
    }
    drop(magasin);
    let relu = MagasinAppairage::ouvrir(&dossier);
    // Reprendre l'enfant avant toute assertion : un echec ne doit pas laisser
    // de processus derriere lui.
    unsafe {
        libc::kill(enfant, libc::SIGKILL);
        libc::waitpid(enfant, std::ptr::null_mut(), 0);
    }
    let relu = relu.expect("le verrou doit etre relache malgre un fork concurrent");
    assert_eq!(
        relu.identite().id(),
        id,
        "le magasin rouvert est bien le meme"
    );
}

#[test]
fn i3326_le_verrou_et_la_reprise_sont_verifies_entre_processus_distincts() {
    let t = tempfile::tempdir().unwrap();
    let dossier = t.path().join("sendspin");
    let mut magasin = MagasinAppairage::ouvrir(&dossier).unwrap();
    let id = magasin.identite().id();
    let client = Identite::generer();
    magasin
        .conserver(&client.id(), &cle(&client, 29), MethodeAppairage::Psk)
        .unwrap();
    let enfant = |mode| {
        let resultat = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "magasin_appairage::i3326_processus_magasin",
                "--ignored",
                "--nocapture",
            ])
            .env("TUNE_TEST_SENDSPIN_STORE", &dossier)
            .env("TUNE_TEST_SENDSPIN_MODE", mode)
            .env("TUNE_TEST_SENDSPIN_SERVER", &id)
            .env("TUNE_TEST_SENDSPIN_CLIENT", client.id())
            .output()
            .unwrap();
        assert!(
            resultat.status.success(),
            "processus {mode} : {} {}",
            String::from_utf8_lossy(&resultat.stdout),
            String::from_utf8_lossy(&resultat.stderr)
        );
    };
    enfant("occupe");
    drop(magasin);
    enfant("reprendre");
}

#[test]
#[ignore = "auxiliaire lance explicitement par le test multiprocessus"]
fn i3326_processus_magasin() {
    let dossier = std::env::var_os("TUNE_TEST_SENDSPIN_STORE").expect("dossier de fixture");
    if std::env::var("TUNE_TEST_SENDSPIN_MODE").unwrap() == "occupe" {
        assert!(
            matches!(
                MagasinAppairage::ouvrir(std::path::Path::new(&dossier)),
                Err(ErreurMagasin::Occupe)
            ),
            "un autre processus ne peut pas ecraser le magasin actif"
        );
    } else {
        let magasin = MagasinAppairage::ouvrir(std::path::Path::new(&dossier)).unwrap();
        assert_eq!(
            magasin.identite().id(),
            std::env::var("TUNE_TEST_SENDSPIN_SERVER").unwrap()
        );
        let client = std::env::var("TUNE_TEST_SENDSPIN_CLIENT").unwrap();
        assert_eq!(
            magasin
                .cle_du_pair(&client)
                .unwrap()
                .expect("la cle persiste dans un nouveau processus")
                .identifiant(),
            psk::identifiant(&[29; 32])
        );
    }
}

#[cfg(unix)]
#[test]
fn i3326_les_permissions_et_les_liens_ne_peuvent_pas_exposer_le_magasin() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let t = tempfile::tempdir().unwrap();
    let dossier = t.path().join("sendspin");
    drop(MagasinAppairage::ouvrir(&dossier).unwrap());
    let fichier = dossier.join("pairing.json");
    assert_eq!(
        fs::metadata(&dossier).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        fs::metadata(&fichier).unwrap().permissions().mode() & 0o777,
        0o600
    );
    fs::set_permissions(&fichier, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(
        MagasinAppairage::ouvrir(&dossier).is_err(),
        "pas de secret lisible par les autres comptes"
    );
    fs::set_permissions(&fichier, fs::Permissions::from_mode(0o600)).unwrap();
    let original = dossier.join("original.json");
    fs::rename(&fichier, &original).unwrap();
    symlink(&original, &fichier).unwrap();
    assert!(
        MagasinAppairage::ouvrir(&dossier).is_err(),
        "ne pas suivre un lien vers les secrets"
    );
    fs::remove_file(&fichier).unwrap();
    fs::hard_link(&original, &fichier).unwrap();
    assert!(
        MagasinAppairage::ouvrir(&dossier).is_err(),
        "un fichier de secrets ne partage pas son inode"
    );
}
