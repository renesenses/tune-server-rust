//! Preuves de T19 (droits et noms) côté TESS : l'égaliseur paramétrique est
//! Premium avec son droit propre (Q3), le graphique et le casque restent
//! gratuits, pour chaque sorte de licence (gratuit, Premium, à vie, essai
//! échu), et les réglages existants migrent sans rien perdre (P2 à la
//! lecture seulement).

use std::sync::Arc;

use serde_json::Value;

use crate::audio::eq::{EqBandSpec, EqProfile};
use crate::db::backend::DbBackend;
use crate::db::settings_repo::SettingsRepo;
use crate::license::{Feature, LicenseManager, Tier};
use crate::tess::api::ReponseCatalogue;
use crate::tess::migration::{ID_EQ_GRAPHIQUE, ID_EQ_PARAMETRIQUE, lire_zone};
use crate::tess::{
    ChaineTess, ContexteValidation, DescripteurGreffon, Droit, ErreurChaine, MaillonGreffon,
    catalogue_de_reference,
};

const PASSE: &str = "2020-01-01T00:00:00Z";

#[derive(Debug, Clone, Copy)]
enum Licence {
    Gratuit,
    Premium,
    AVie,
    EssaiEchuParLaCle,
    EssaiEchuParLeCompte,
}

const TOUTES: [Licence; 5] = [
    Licence::Gratuit,
    Licence::Premium,
    Licence::AVie,
    Licence::EssaiEchuParLaCle,
    Licence::EssaiEchuParLeCompte,
];

impl Licence {
    fn attendu_premium(self) -> bool {
        matches!(self, Licence::Premium | Licence::AVie)
    }
}

async fn base(licence: Licence) -> (LicenseManager, SettingsRepo) {
    use crate::db::migrations;
    use crate::db::sqlite::SqliteDb;
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    migrations::run_migrations(&db).unwrap();
    let backend: Arc<dyn DbBackend> = Arc::new(db);
    let mgr = LicenseManager::new(backend.clone());
    match licence {
        Licence::Gratuit => {}
        Licence::Premium => {
            mgr.update_from_server(Tier::Premium, Some("2099-12-31T00:00:00Z".into()))
                .await
        }
        Licence::AVie => mgr.update_from_server(Tier::Premium, None).await,
        Licence::EssaiEchuParLaCle => {
            mgr.update_from_server(Tier::Premium, Some(PASSE.into()))
                .await
        }
        Licence::EssaiEchuParLeCompte => mgr.set_account_premium(true, Some(PASSE.into())).await,
    }
    (mgr, SettingsRepo::with_backend(backend))
}

fn maillon(id: &str, enabled: bool) -> MaillonGreffon {
    MaillonGreffon {
        id: id.into(),
        enabled,
        settings: Value::Null,
    }
}

fn chaine(plugins: Vec<MaillonGreffon>) -> ChaineTess {
    ChaineTess {
        plugins,
        ..ChaineTess::default()
    }
}

fn parametrique() -> EqProfile {
    let b = |freq: f64, gain: f64, q: f64, t: &str| EqBandSpec {
        freq,
        gain,
        q,
        band_type: t.into(),
        channel: None,
    };
    EqProfile {
        enabled: true,
        bands: vec![
            b(80.0, 4.0, 0.7, "low_shelf"),
            b(1200.0, -3.5, 1.4, "peak"),
            b(9000.0, -2.0, 0.7, "high_shelf"),
        ],
        ..EqProfile::default()
    }
}

fn tout(s: &SettingsRepo) -> Vec<(String, String)> {
    let mut v = s.all().unwrap();
    v.sort();
    v
}

#[tokio::test]
async fn chaque_licence_parametrique_402_sans_premium_graphique_et_casque_toujours() {
    let catalogue = catalogue_de_reference();
    for licence in TOUTES {
        let (mgr, _) = base(licence).await;
        let premium = mgr.is_premium().await;
        assert_eq!(premium, licence.attendu_premium(), "{licence:?}");
        assert_eq!(
            mgr.check_feature(Feature::ParametricEq).await,
            premium,
            "{licence:?}"
        );
        let ctx = ContexteValidation { premium };

        // Graphique et casque : gratuits pour tous (chacun seul : les
        // façades de l'égaliseur sont exclusives depuis la rc14).
        for id in ["equalizer-graphic", "equalizer-headphone"] {
            chaine(vec![maillon(id, true)])
                .valider(&catalogue, ctx)
                .unwrap_or_else(|e| panic!("{licence:?} {id} : {e}"));
        }

        // Paramétrique coché : 402 sans Premium, accepté avec.
        let peq = chaine(vec![maillon(ID_EQ_PARAMETRIQUE, true)]);
        match peq.valider(&catalogue, ctx) {
            Ok(()) => assert!(premium, "{licence:?} : paramétrique accordé sans Premium"),
            Err(e) => {
                assert!(!premium, "{licence:?} : {e}");
                assert_eq!(e, ErreurChaine::PremiumRequis(ID_EQ_PARAMETRIQUE.into()));
                assert_eq!(e.statut_http(), 402);
            }
        }
        // Décoché : gardé sans droit.
        chaine(vec![maillon(ID_EQ_PARAMETRIQUE, false)])
            .valider(&catalogue, ctx)
            .unwrap();

        // Le catalogue rendu au client dit la même chose.
        let rep = ReponseCatalogue::depuis(catalogue.clone(), premium, &|_| true);
        let etat = |id: &str| {
            rep.plugins
                .iter()
                .find(|e| e.id == id)
                .unwrap_or_else(|| panic!("{id}"))
                .entitled
        };
        assert!(etat("equalizer-graphic"), "{licence:?}");
        assert!(etat("equalizer-headphone"), "{licence:?}");
        assert_eq!(etat(ID_EQ_PARAMETRIQUE), premium, "{licence:?}");
    }
}

/// Migration des réglages existants : un égaliseur paramétrique enregistré
/// avant T19 joue tel quel pour une licence Premium ou à vie ; pour un compte
/// gratuit ou un essai échu, il devient un graphique au plus près, à la
/// lecture seulement : aucune clé n'est écrite, et le Premium retrouvé rend le
/// paramétrique.
#[tokio::test]
async fn chaque_licence_le_parametrique_existant_migre_sans_perte() {
    for licence in TOUTES {
        let (mgr, s) = base(licence).await;
        let z = 4;
        s.set(
            &format!("zone_{z}_eq_profile"),
            &serde_json::to_string(&parametrique()).unwrap(),
        )
        .unwrap();
        let avant = tout(&s);
        let premium = mgr.is_premium().await;
        let l = lire_zone(&s, z, premium).unwrap();
        let id = l.chaine.plugins[0].id.as_str();
        if licence.attendu_premium() {
            assert!(!l.conversion_p2, "{licence:?}");
            assert_eq!(id, ID_EQ_PARAMETRIQUE, "{licence:?}");
        } else {
            assert!(l.conversion_p2, "{licence:?}");
            assert_eq!(id, ID_EQ_GRAPHIQUE, "{licence:?}");
            assert!(
                l.chaine.plugins[0].enabled,
                "{licence:?} : le son a disparu"
            );
        }
        assert_eq!(avant, tout(&s), "{licence:?} : la lecture a écrit");
        // Premium retrouvé : le paramétrique d'origine revient.
        let retour = lire_zone(&s, z, true).unwrap();
        assert_eq!(
            retour.chaine.plugins[0].id, ID_EQ_PARAMETRIQUE,
            "{licence:?}"
        );
    }
}

/// Le lien fin droit ↔ `Feature` ne fait que S'AJOUTER au droit déclaré.
#[test]
fn la_feature_citee_ne_rend_jamais_gratuit_un_greffon_premium() {
    let modele = catalogue_de_reference()
        .get("equalizer-graphic")
        .unwrap()
        .clone();
    let avec = |entitlement: Droit, feature: Option<&str>| DescripteurGreffon {
        id: "greffon-tiers".into(),
        entitlement,
        feature: feature.map(str::to_owned),
        ..modele.clone()
    };
    // Un paquet qui se dit gratuit mais cite un droit Premium : Premium.
    assert!(avec(Droit::Free, Some("parametric_eq")).demande_le_premium());
    assert!(avec(Droit::Free, Some("room_correction")).demande_le_premium());
    // Un greffon Premium qui cite un droit gratuit ou inconnu : Premium.
    assert!(avec(Droit::Premium, Some("dsp_eq")).demande_le_premium());
    assert!(avec(Droit::Premium, Some("no_such_feature")).demande_le_premium());
    assert!(avec(Droit::Unknown, None).demande_le_premium());
    // Gratuit sans droit Premium cité : gratuit.
    assert!(!avec(Droit::Free, None).demande_le_premium());
    assert!(!avec(Droit::Free, Some("dsp_eq")).demande_le_premium());
    assert!(!avec(Droit::Free, Some("no_such_feature")).demande_le_premium());
    // Et la validation d'une chaîne suit ce lien.
    let mut c = catalogue_de_reference();
    c.plugins.push(avec(Droit::Free, Some("parametric_eq")));
    let e = chaine(vec![maillon("greffon-tiers", true)])
        .valider(&c, ContexteValidation { premium: false })
        .unwrap_err();
    assert_eq!(e, ErreurChaine::PremiumRequis("greffon-tiers".into()));
}
