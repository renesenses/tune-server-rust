//! Témoins du dépôt des clés clientes Subsonic (#7855).

use std::sync::Arc;

use super::*;
use crate::db::backend::DbBackend;
use crate::db::migrations;
use crate::db::sqlite::SqliteDb;

fn base() -> Arc<dyn DbBackend> {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    migrations::run_migrations(&db).unwrap();
    Arc::new(db)
}

#[test]
fn la_base_ne_garde_que_l_empreinte_de_la_cle_7855() {
    let db = base();
    let repo = ClesClientsRepo::with_backend(db.clone());
    let creee = repo.creer(1, "Symfonium").unwrap();
    assert!(creee.secret.starts_with(PREFIXE_CLE));

    let lignes = db.query_many("SELECT * FROM cles_clients", &[]).unwrap();
    assert_eq!(lignes.len(), 1);
    for valeur in &lignes[0] {
        if let Some(texte) = valeur.as_str() {
            assert!(
                !texte.contains(&creee.secret),
                "la clé en clair ne doit jamais être stockée : {texte}"
            );
        }
    }
    let stockee = db
        .query_one("SELECT empreinte_sha256 FROM cles_clients", &[])
        .unwrap()
        .unwrap();
    assert_eq!(stockee[0].as_str(), Some(empreinte(&creee.secret).as_str()));
}

#[test]
fn une_cle_valide_s_authentifie_et_une_cle_inconnue_non_7855() {
    let repo = ClesClientsRepo::with_backend(base());
    let creee = repo.creer(1, "DSub").unwrap();
    let trouvee = repo.authentifier(&creee.secret).unwrap();
    assert_eq!(trouvee.map(|c| c.id), Some(creee.cle.id.clone()));

    assert!(repo.authentifier("tune_sk_inconnue").unwrap().is_none());
    assert!(repo.authentifier("").unwrap().is_none());
}

#[test]
fn une_cle_revoquee_ne_s_authentifie_plus_7855() {
    let repo = ClesClientsRepo::with_backend(base());
    let creee = repo.creer(1, "Feishin").unwrap();
    assert!(repo.revoquer(1, &creee.cle.id).unwrap());
    assert!(
        repo.authentifier(&creee.secret).unwrap().is_none(),
        "une clé révoquée doit être refusée"
    );
    let liste = repo.lister(1).unwrap();
    assert_eq!(liste.len(), 1);
    assert!(liste[0].revoquee_le.is_some());
}

#[test]
fn un_profil_ne_revoque_ni_ne_liste_les_cles_d_un_autre_7855() {
    let repo = ClesClientsRepo::with_backend(base());
    let creee = repo.creer(1, "Supersonic").unwrap();
    assert!(!repo.revoquer(2, &creee.cle.id).unwrap());
    assert!(repo.lister(2).unwrap().is_empty());
    assert!(repo.authentifier(&creee.secret).unwrap().is_some());
}

#[test]
fn deux_cles_sont_differentes_7855() {
    let repo = ClesClientsRepo::with_backend(base());
    let a = repo.creer(1, "a").unwrap();
    let b = repo.creer(1, "b").unwrap();
    assert_ne!(a.secret, b.secret);
    assert_ne!(a.cle.id, b.cle.id);
}

#[test]
fn la_cle_d_un_profil_supprime_ne_s_authentifie_plus_7855() {
    let db = base();
    db.execute(
        "INSERT INTO profiles (id, username) VALUES (2, 'banc7855')",
        &[],
    )
    .unwrap();
    let repo = ClesClientsRepo::with_backend(db.clone());
    let creee = repo.creer(2, "Symfonium").unwrap();
    assert!(repo.authentifier(&creee.secret).unwrap().is_some());

    db.execute("DELETE FROM profiles WHERE id = 2", &[])
        .unwrap();
    assert!(
        repo.authentifier(&creee.secret).unwrap().is_none(),
        "la clé d'un profil supprimé doit être refusée"
    );
}
