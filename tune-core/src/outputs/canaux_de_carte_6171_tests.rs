//! Témoins de #6171 — la correspondance des canaux d'une zone ASIO, en test
//! pur : le rappel ASIO lui-même ne se compile que sous Windows avec `asio`.
use super::*;
use serde_json::json;

fn canaux(numeros: &[u16]) -> CanauxDeCarte {
    CanauxDeCarte::depuis_numeros(numeros).unwrap().unwrap()
}

/// Un anneau de test : rend ses mots dans l'ordre, puis plus rien.
fn anneau(mots: Vec<i32>) -> impl FnMut(&mut [i32]) -> usize {
    let mut pos = 0;
    move |sortie: &mut [i32]| {
        let n = sortie.len().min(mots.len() - pos);
        sortie[..n].copy_from_slice(&mots[pos..pos + n]);
        pos += n;
        n
    }
}

#[test]
fn le_reglage_se_lit_a_partir_de_1_et_refuse_ce_qui_ne_se_relit_pas() {
    assert_eq!(CanauxDeCarte::depuis_json(&json!(null)), Ok(None));
    assert_eq!(CanauxDeCarte::depuis_json(&json!([])), Ok(None));
    let c = CanauxDeCarte::depuis_json(&json!([3, 4])).unwrap().unwrap();
    assert_eq!(c.indices(), &[2, 3]);
    assert_eq!(c.numeros(), vec![3, 4]);
    assert_eq!(c.largeur(), 4);

    assert_eq!(
        CanauxDeCarte::depuis_json(&json!("3,4")),
        Err(ErreurDeCanaux::Forme)
    );
    for mauvais in [
        json!([0]),
        json!([-1]),
        json!([1.5]),
        json!(["3"]),
        json!([513]),
    ] {
        assert!(
            matches!(
                CanauxDeCarte::depuis_json(&mauvais),
                Err(ErreurDeCanaux::HorsBorne(_))
            ),
            "{mauvais}"
        );
    }
    assert_eq!(
        CanauxDeCarte::depuis_json(&json!([3, 4, 3])),
        Err(ErreurDeCanaux::Doublon(3))
    );
}

/// Rien de réglé, ou les premiers canaux dans l'ordre : aucun plan, le chemin
/// d'avant au mot près.
#[test]
fn sans_reglage_ou_identite_aucun_plan() {
    assert_eq!(planifier(None, 2, Some(4)), Ok(None));
    assert_eq!(planifier(Some(&canaux(&[1, 2])), 2, Some(4)), Ok(None));
    // [1, 2, 3, 4] pour une piste stéréo : les deux premiers seulement servent.
    assert_eq!(
        planifier(Some(&canaux(&[1, 2, 3, 4])), 2, Some(4)),
        Ok(None)
    );
}

/// Le casque de Jean-François : une carte à quatre sorties, la piste stéréo
/// sur 3-4. La carte s'ouvre sur quatre canaux, 1-2 reçoivent des zéros.
#[test]
fn casque_sur_les_canaux_3_et_4() {
    let plan = planifier(Some(&canaux(&[3, 4])), 2, Some(4))
        .unwrap()
        .expect("un plan");
    assert_eq!(plan.largeur(), 4);
    assert_eq!(plan.cible(), &[2, 3]);

    let mut d = Some(Dispersion::new(plan, 0i32));
    // Trois trames stéréo : (G, D) = (11, 12), (21, 22), (31, 32).
    let mut tirer = anneau(vec![11, 12, 21, 22, 31, 32]);
    let mut data = vec![-7i32; 3 * 4];
    let famine = remplir_le_tampon(&mut data, &mut d, 0, &mut tirer);
    assert!(!famine);
    assert_eq!(data, vec![0, 0, 11, 12, 0, 0, 21, 22, 0, 0, 31, 32]);
}

/// Le 7.1 réordonné et non consécutif : chaque canal de la piste va à son
/// numéro, les trous restent muets.
#[test]
fn multicanal_non_consecutif_et_reordonne() {
    // Piste 4 canaux (FL FR BL BR) vers les canaux 2, 1, 7, 5 de la carte.
    let plan = planifier(Some(&canaux(&[2, 1, 7, 5])), 4, Some(8))
        .unwrap()
        .unwrap();
    assert_eq!(plan.largeur(), 7);
    let mut d = Some(Dispersion::new(plan, 0i32));
    let mut tirer = anneau(vec![1, 2, 3, 4, 5, 6, 7, 8]);
    let mut data = vec![9i32; 2 * 7];
    assert!(!remplir_le_tampon(&mut data, &mut d, 0, &mut tirer));
    assert_eq!(
        data,
        vec![
            2, 1, 0, 0, 4, 0, 3, //
            6, 5, 0, 0, 8, 0, 7,
        ]
    );
}

/// Le casque en 17-18 après un 9.1.6 : la largeur suit le plus grand numéro.
#[test]
fn casque_en_17_18_sur_une_carte_a_64_sorties() {
    let plan = planifier(Some(&canaux(&[17, 18])), 2, Some(64))
        .unwrap()
        .unwrap();
    assert_eq!(plan.largeur(), 18);
    let mut d = Some(Dispersion::new(plan, 0i32));
    let mut tirer = anneau(vec![5, 6]);
    let mut data = vec![1i32; 18];
    remplir_le_tampon(&mut data, &mut d, 0, &mut tirer);
    let mut attendu = vec![0i32; 18];
    attendu[16] = 5;
    attendu[17] = 6;
    assert_eq!(data, attendu);
}

/// Une piste qui a moins de canaux que la zone n'en vise : ses canaux vont aux
/// premiers numéros, les suivants restent muets. Une piste qui en a PLUS est
/// refusée : Tune ne jette pas un canal sans le dire.
#[test]
fn trop_de_canaux_refuse_moins_de_canaux_accepte() {
    let p = planifier(Some(&canaux(&[3, 4])), 1, Some(4))
        .unwrap()
        .unwrap();
    assert_eq!(p.cible(), &[2]);
    assert_eq!(p.largeur(), 3);

    let refus = planifier(Some(&canaux(&[3, 4])), 6, Some(8)).unwrap_err();
    assert!(
        refus.contains("6 canaux") && refus.contains("3, 4"),
        "{refus}"
    );
    let refus = planifier(Some(&canaux(&[17, 18])), 2, Some(8)).unwrap_err();
    assert!(
        refus.contains("canal 18") && refus.contains("8 sorties"),
        "{refus}"
    );
}

/// Une famine de l'anneau se signale, et la fin du tampon reste muette. Un
/// grand tampon (plus que `TRAMES_PAR_PASSE`) est traité en plusieurs passes
/// sans perdre une trame.
#[test]
fn famine_et_grands_tampons() {
    let plan = planifier(Some(&canaux(&[2, 1])), 2, Some(2))
        .unwrap()
        .expect("l'échange gauche/droite est un plan");
    let trames = TRAMES_PAR_PASSE * 2 + 3;
    let source: Vec<i32> = (0..(trames as i32 * 2)).collect();
    let mut d = Some(Dispersion::new(plan.clone(), 0i32));
    let mut tirer = anneau(source.clone());
    let mut data = vec![0i32; trames * 2];
    assert!(!remplir_le_tampon(&mut data, &mut d, 0, &mut tirer));
    for t in 0..trames {
        assert_eq!(data[t * 2], source[t * 2 + 1]);
        assert_eq!(data[t * 2 + 1], source[t * 2]);
    }

    let mut d = Some(Dispersion::new(plan, 0i32));
    let mut tirer = anneau(vec![1, 2]);
    let mut data = vec![7i32; 6];
    assert!(
        remplir_le_tampon(&mut data, &mut d, 0, &mut tirer),
        "famine"
    );
    assert_eq!(data, vec![2, 1, 0, 0, 0, 0]);
}

/// Sans dispersion, le chemin d'avant : l'anneau remplit, le reste est zéro.
#[test]
fn sans_dispersion_le_chemin_d_avant() {
    let mut d: Option<Dispersion<i32>> = None;
    let mut tirer = anneau(vec![1, 2, 3]);
    let mut data = vec![9i32; 4];
    assert!(remplir_le_tampon(&mut data, &mut d, 0, &mut tirer));
    assert_eq!(data, vec![1, 2, 3, 0]);
}

/// Le réglage se relit depuis la base, et une valeur illisible vaut « rien ».
#[test]
fn le_reglage_se_relit_depuis_la_base() {
    let db = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    let backend: std::sync::Arc<dyn crate::db::backend::DbBackend> = std::sync::Arc::new(db);
    let settings = crate::db::settings_repo::SettingsRepo::with_backend(backend.clone());
    assert_eq!(CanauxDeCarte::lire(&backend, 7), None);
    settings.set(&cle_zone(7), "[3,4]").unwrap();
    assert_eq!(CanauxDeCarte::lire(&backend, 7), Some(canaux(&[3, 4])));
    settings.set(&cle_zone(7), "\"3-4\"").unwrap();
    assert_eq!(CanauxDeCarte::lire(&backend, 7), None);
}
