//! Témoins de la base OPRA (rc14, greffon TESS « Casque »).

use std::sync::Mutex;

use async_trait::async_trait;

use super::*;

/// Un extrait de `database_v1.jsonl` : les lignes de Sennheiser HD650 sont
/// celles de la base publiée (commit 0b88ecd, bandes abrégées), dans le
/// désordre où la base peut les servir (correction avant son produit).
pub(crate) const EXTRAIT: &str = r#"{"type":"eq","id":"sennheiser:hd650::oratory1990_harman_target","data":{"author":"oratory1990","details":"Harman Target","link":"https://example.invalid/hd650.pdf","type":"parametric_eq","product_id":"sennheiser::hd650","parameters":{"gain_db":-9.3,"bands":[{"type":"peak_dip","frequency":20,"gain_db":4,"q":1.1},{"type":"peak_dip","frequency":97,"gain_db":-2.5,"q":0.7},{"type":"low_shelf","frequency":105,"gain_db":5.5,"q":0.71},{"type":"high_shelf","frequency":10000,"gain_db":-3,"q":0.71},{"type":"high_pass","frequency":15,"slope":12}]}}}
{"type":"vendor","id":"sennheiser","data":{"name":"Sennheiser"}}
{"type":"product","id":"sennheiser::hd650","data":{"name":"HD650","type":"headphones","subtype":"over_the_ear","vendor_id":"sennheiser"}}
{"type":"eq","id":"sennheiser:hd650::autoeq_crinacle","data":{"author":"AutoEQ","details":"Measured by crinacle","type":"parametric_eq","product_id":"sennheiser::hd650","parameters":{"gain_db":-6,"bands":[{"type":"low_shelf","frequency":105,"gain_db":6,"q":0.7}]}}}
{"type":"product","id":"sennheiser::hd600","data":{"name":"HD600","type":"headphones","subtype":"over_the_ear","vendor_id":"sennheiser"}}
{"type":"eq","id":"sennheiser:hd600::autoeq_oratory1990","data":{"author":"AutoEQ","details":"Measured by oratory1990","type":"parametric_eq","product_id":"sennheiser::hd600","parameters":{"gain_db":-5,"bands":[{"type":"band_pass","frequency":1000,"gain_db":3,"q":1}]}}}
{"type":"vendor","id":"beyerdynamic","data":{"name":"Beyerdynamic"}}
{"type":"product","id":"beyerdynamic::dt_770_pro_80","data":{"name":"DT 770 PRO 80 Ohm","type":"headphones","subtype":"over_the_ear","vendor_id":"beyerdynamic"}}
{"type":"eq","id":"beyerdynamic:dt_770_pro_80::oratory1990_harman_target","data":{"author":"oratory1990","details":"Harman Target","type":"parametric_eq","product_id":"beyerdynamic::dt_770_pro_80","parameters":{"gain_db":-7,"bands":[{"type":"peak_dip","frequency":5000,"gain_db":-4,"q":2}]}}}
{"type":"product","id":"beyerdynamic::sans_correction","data":{"name":"Sans correction","type":"headphones","subtype":"on_ear","vendor_id":"beyerdynamic"}}
ceci n'est pas du JSON
{"type":"eq","id":"orphelin::x","data":{"author":"x","details":"y","type":"parametric_eq","product_id":"inconnu::x","parameters":{"gain_db":0,"bands":[]}}}
"#;

#[test]
fn la_base_resout_marques_et_variantes_et_ecarte_le_reste() {
    let base = analyser_base(EXTRAIT);
    // Le produit sans correction et la correction orpheline sont écartés ;
    // la ligne illisible est sautée sans perdre le reste.
    assert_eq!(base.produits.len(), 3);
    assert_eq!(base.nb_corrections(), 4);
    let hd650 = base
        .produits
        .iter()
        .find(|p| p.id == "sennheiser::hd650")
        .unwrap();
    // La marque est résolue même quand sa ligne arrive après la correction.
    assert_eq!(hd650.vendor, "Sennheiser");
    assert_eq!(hd650.form, "over_the_ear");
    let ids: Vec<&str> = hd650.variants.iter().map(|v| v.id.as_str()).collect();
    assert_eq!(
        ids,
        [
            "sennheiser:hd650::oratory1990_harman_target",
            "sennheiser:hd650::autoeq_crinacle"
        ]
    );
    assert_eq!(hd650.variants[0].author, "oratory1990");
    assert_eq!(hd650.variants[0].details, "Harman Target");
    assert!(hd650.variants[0].link.is_some());
    assert!(hd650.variants[1].link.is_none());
}

#[test]
fn la_recherche_trouve_par_marque_modele_et_variante() {
    let base = analyser_base(EXTRAIT);
    let ids = |q: &str| -> Vec<String> {
        rechercher(&base, q, 50)
            .0
            .into_iter()
            .map(|p| p.id.clone())
            .collect()
    };
    // Le modèle exact passe devant ; l'écriture compacte trouve aussi.
    assert_eq!(ids("hd650")[0], "sennheiser::hd650");
    assert_eq!(ids("HD 650")[0], "sennheiser::hd650");
    assert_eq!(ids("sennheiser hd650"), ["sennheiser::hd650"]);
    // Marque seule : ses deux casques corrigés.
    assert_eq!(ids("sennheiser").len(), 2);
    // Variante : l'auteur et la cible sont cherchés aussi.
    assert_eq!(ids("dt 770 harman"), ["beyerdynamic::dt_770_pro_80"]);
    assert_eq!(ids("crinacle"), ["sennheiser::hd650"]);
    // Sans accent ni casse.
    assert_eq!(ids("BÈYERDYNAMIC")[0], "beyerdynamic::dt_770_pro_80");
    // Rien, et une requête vide ne rend rien.
    assert!(ids("zzz").is_empty());
    assert_eq!(rechercher(&base, "  ", 50).1, 0);
    // La limite borne la liste, pas le total.
    let (liste, total) = rechercher(&base, "sennheiser", 1);
    assert_eq!((liste.len(), total), (1, 2));
}

#[test]
fn une_correction_se_lit_en_bandes_du_moteur() {
    let base = analyser_base(EXTRAIT);
    let c = correction_de(&base, "sennheiser:hd650::oratory1990_harman_target").unwrap();
    assert_eq!(c.produit.name, "HD650");
    assert_eq!(c.produit.vendor, "Sennheiser");
    assert!(c.produit.variants.is_empty());
    assert_eq!(c.variante.author, "oratory1990");
    let types: Vec<&str> = c
        .profil
        .bandes
        .iter()
        .map(|b| b.band_type.as_str())
        .collect();
    assert_eq!(
        types,
        ["peak", "peak", "low_shelf", "high_shelf", "high_pass"]
    );
    assert_eq!(c.profil.bandes[2].gain, 5.5);
    assert_eq!(c.profil.preamp_db, -9.3);
}

#[test]
fn une_correction_inconnue_ou_injouable_est_refusee_et_nommee() {
    let base = analyser_base(EXTRAIT);
    let e = correction_de(&base, "sennheiser:hd650::nulle_part").unwrap_err();
    assert_eq!(e.cle(), "opra_profile_unknown");
    // `band_pass` : Tune refuse plutôt que de jouer une autre correction.
    let e = correction_de(&base, "sennheiser:hd600::autoeq_oratory1990").unwrap_err();
    assert_eq!(e.cle(), "opra_profile_unsupported");
    assert!(matches!(
        e,
        ErreurBaseOpra::ProfilNonPrisEnCharge {
            cle: "unknown_filter_type",
            ..
        }
    ));
}

/// Un faux réseau : rend `reponse` (ou l'erreur) et compte les appels.
struct FauxReseau {
    reponse: Mutex<Result<String, String>>,
    appels: Mutex<usize>,
}

impl FauxReseau {
    fn new(r: Result<&str, &str>) -> Arc<Self> {
        Arc::new(Self {
            reponse: Mutex::new(r.map(str::to_owned).map_err(str::to_owned)),
            appels: Mutex::new(0),
        })
    }
    fn couper(&self) {
        *self.reponse.lock().unwrap() = Err("réseau coupé".into());
    }
    fn appels(&self) -> usize {
        *self.appels.lock().unwrap()
    }
}

#[async_trait]
impl TelechargeurAutoEq for FauxReseau {
    async fn telecharger(&self, _url: &str, _taille_max: usize) -> Result<String, String> {
        *self.appels.lock().unwrap() += 1;
        self.reponse.lock().unwrap().clone()
    }
}

#[tokio::test]
async fn hors_ligne_sans_copie_l_erreur_le_dit() {
    let reseau = FauxReseau::new(Err("pas de réseau"));
    let cat = CatalogueOpra::new("http://opra.invalid/db.jsonl", reseau, None);
    let e = cat.rechercher("hd650", 10).await.unwrap_err();
    assert_eq!(e.cle(), "opra_unreachable");
}

#[tokio::test]
async fn la_copie_locale_sert_quand_le_reseau_tombe() {
    let dossier = tempfile::tempdir().unwrap();
    let reseau = FauxReseau::new(Ok(EXTRAIT));
    let cat = CatalogueOpra::new(
        "http://opra.invalid/db.jsonl",
        reseau.clone(),
        Some(dossier.path().to_path_buf()),
    );
    let (r, _) = cat.rechercher("hd650", 10).await.unwrap();
    assert_eq!(r[0].id, "sennheiser::hd650");
    assert!(dossier.path().join(FICHIER_BASE).exists());

    // Un serveur neuf, réseau coupé, base périmée : la copie sert.
    reseau.couper();
    let cat = CatalogueOpra::new(
        "http://opra.invalid/db.jsonl",
        reseau.clone(),
        Some(dossier.path().to_path_buf()),
    )
    .avec_fraicheur(Duration::ZERO);
    let c = cat
        .correction("beyerdynamic:dt_770_pro_80::oratory1990_harman_target")
        .await
        .unwrap();
    assert_eq!(c.produit.vendor, "Beyerdynamic");
}

#[tokio::test]
async fn une_base_fraiche_ne_retourne_pas_sur_le_reseau() {
    let reseau = FauxReseau::new(Ok(EXTRAIT));
    let cat = CatalogueOpra::new("http://opra.invalid/db.jsonl", reseau.clone(), None);
    cat.rechercher("hd650", 10).await.unwrap();
    cat.correction("sennheiser:hd650::autoeq_crinacle")
        .await
        .unwrap();
    assert_eq!(reseau.appels(), 1);
}

#[tokio::test]
async fn une_reponse_qui_n_est_pas_la_base_est_refusee() {
    let reseau = FauxReseau::new(Ok("<html>404</html>"));
    let cat = CatalogueOpra::new("http://opra.invalid/db.jsonl", reseau, None);
    assert_eq!(
        cat.rechercher("hd650", 10).await.unwrap_err().cle(),
        "opra_database_unreadable"
    );
}
