//! Échelle de volume : le SEUL endroit où l'on passe du facteur linéaire aux
//! décibels, et l'inverse.
//!
//! ## Pourquoi ce module existe (#1274)
//!
//! Le volume de Tune est un multiplicateur linéaire appliqué à chaque
//! échantillon (`outputs::local::effective_volume_units`). L'API l'expose tel
//! quel, sur `0.0..=1.0`, et l'interface le présente en pourcentage. Or un
//! pourcentage linéaire ne dit rien à qui règle une chaîne : un curseur à 90 %
//! vaut −0,9 dB, pas −10 dB, et à 50 % on n'a pas « moitié moins fort » mais
//! −6 dB. Sur le forum, `zaurux` (Roon + HQPlayer + Diretta) l'a résumé en cinq
//! mots : « pas de réglage au dB près ».
//!
//! Ce module ne change PAS la loi de volume. Il n'introduit ni courbe
//! perceptuelle, ni conique, ni plancher : ce que 100 % veut dire aujourd'hui,
//! il le veut dire après. Il donne seulement à ce même nombre sa lecture
//! d'audiophile, l'atténuation en dB par rapport à la pleine échelle.
//!
//! ## Trois règles qu'aucune évolution ne doit casser
//!
//! 1. **Le zéro est le silence, pas un plancher.** `linear_to_db(0.0)` rend
//!    `None`, qui se sérialise en `null`. Rendre `-60` ou `-120` inventerait
//!    une atténuation finie là où il n'y a plus de son, et un client qui
//!    renverrait cette valeur rallumerait la zone.
//! 2. **La conversion est exacte et réversible.** Aucun arrondi n'est fait ici.
//!    Arrondir à 0,1 dB ferait dériver le volume à chaque aller-retour
//!    lecture → écriture ; le formatage pour l'affichage appartient au client.
//! 3. **Le plafond est l'unité.** `db_to_linear` refuse de dépasser 0 dB, comme
//!    `effective_volume_units` borne déjà le produit à 1.0 : au-dessus, on ne
//!    monte pas le son, on écrête.
//!
//! Ce module ne connaît ni le ReplayGain, ni le trim de gain par renderer
//! (`zone_{id}_gain_trim_db`), ni le mode PURE. Il convertit le volume
//! **utilisateur**, celui du curseur, et rien d'autre — le champ `volume_db`
//! d'une charge utile est donc toujours la lecture en dB du champ `volume`
//! qui l'accompagne, jamais celle du gain réellement appliqué.

/// Plafond de l'échelle : 0 dB, l'unité. Au-delà, il n'y a pas de volume en
/// plus, seulement de l'écrêtage.
pub const MAX_DB: f64 = 0.0;

/// Atténuation en dB d'un facteur de volume linéaire.
///
/// `None` signifie **silence** (−∞ dB) : c'est le cas de `0.0`, et aussi celui
/// d'une valeur négative ou `NaN`, qui ne peut pas être un volume audible.
///
/// L'entrée est bornée à `1.0` avant le logarithme, comme partout ailleurs
/// dans la chaîne : un état interne qui déborderait afficherait sinon un gain
/// positif que personne n'entendra jamais.
pub fn linear_to_db(linear: f64) -> Option<f64> {
    if !(linear > 0.0) {
        // Couvre 0.0, les négatifs et NaN — la négation est volontaire.
        return None;
    }
    Some(20.0 * linear.min(1.0).log10())
}

/// Facteur de volume linéaire pour une atténuation en dB.
///
/// `None` pour un `NaN` : c'est un refus, pas un silence, et l'appelant doit
/// répondre 400 plutôt que couper le son. `-inf` en revanche est légitime et
/// vaut exactement `0.0`, le silence.
///
/// Une valeur au-dessus de `MAX_DB` est ramenée à l'unité. Aucun plancher :
/// −200 dB rend un nombre minuscule mais positif, et c'est correct — le
/// silence se demande avec `volume: 0` ou avec le mute, pas avec un dB assez
/// bas pour ressembler à zéro.
pub fn db_to_linear(db: f64) -> Option<f64> {
    if db.is_nan() {
        return None;
    }
    if db == f64::NEG_INFINITY {
        return Some(0.0);
    }
    Some(10f64.powf(db.min(MAX_DB) / 20.0))
}

/// Résout le volume demandé par une requête qui parle en linéaire **ou** en dB.
///
/// C'est le point d'entrée unique de l'écriture, symétrique de
/// [`inserer_volume`] côté lecture : les quatre routes de volume du serveur
/// (POST et PUT `/zones/{id}/volume`, PATCH `/zones/{id}`, et le groupe)
/// n'ont pas la même convention historique pour `volume` — 0..1 pour le web,
/// 0..100 pour le widget, entier 0..100 pour le PATCH. Chacune ramène donc
/// **elle-même** son `volume` sur 0..1 avant d'appeler ici ; ce qui est
/// partagé, et qui doit l'être, c'est l'arbitrage entre les deux champs et la
/// conversion des dB.
///
/// Les deux champs sont **exclusifs**. Les accepter ensemble obligerait à
/// inventer un gagnant, et le perdant serait silencieusement ignoré : sur un
/// réglage de volume, c'est la recette d'un niveau surprise.
pub fn demande_lineaire(lineaire: Option<f64>, db: Option<f64>) -> Result<f64, &'static str> {
    match (lineaire, db) {
        (Some(_), Some(_)) => Err("volume et volume_db sont exclusifs — n'en envoyer qu'un"),
        (None, None) => Err("volume ou volume_db est requis"),
        (Some(v), None) => Ok(v.clamp(0.0, 1.0)),
        // Un dB positif est REFUSÉ, pas ramené au plafond : le ramener ferait
        // croire au client qu'il a obtenu son +3 dB. Il n'y a pas de gain
        // au-dessus de la pleine échelle, il n'y a que de l'écrêtage.
        (None, Some(db)) if db > MAX_DB => Err("volume_db doit être négatif ou nul (0 dB = 100 %)"),
        (None, Some(db)) => db_to_linear(db).ok_or("volume_db n'est pas un nombre"),
    }
}

/// Pose `volume` **et** `volume_db` dans une charge utile de zone.
///
/// Les deux champs sortent d'ici ensemble, à partir du même nombre. C'est la
/// seule garantie qu'ils ne puissent pas se contredire : les payloads de zone
/// n'émettent pas tous la même valeur (certaines lisent l'état de lecture,
/// d'autres la colonne `zones.volume` arrondie au pour-cent), et un `volume_db`
/// recalculé depuis une autre source afficherait un dB qui ne correspond pas
/// au curseur rendu dans la même réponse.
pub fn inserer_volume(obj: &mut serde_json::Map<String, serde_json::Value>, linear: f64) {
    obj.insert("volume".into(), serde_json::json!(linear));
    obj.insert("volume_db".into(), serde_json::json!(linear_to_db(linear)));
}

/// Motif de refus d'une consigne en dB qu'une sortie ne sait pas tenir (#1274).
///
/// `None` = la consigne passe. C'est le cas de toute sortie au réglage continu
/// ou en dB, et de toute consigne au-dessus du premier pas d'une grille : ce
/// garde-fou ne mord QUE là où le niveau demandé n'existe pas sur le fil.
///
/// Le cas qu'il attrape n'est pas une imprécision, c'est une extinction. Une
/// sortie DLNA, OpenHome, BluOS, Squeezebox, HQPlayer ou OAAT ne reçoit qu'un
/// entier 0..100 : `−50 dB` vaut 0,00316 en linéaire, l'entier envoyé est
/// `round(0,316) = 0`, et le renderer se tait. Le serveur, lui, gardait la
/// valeur exacte, la persistait, et répondait `200` — la zone était annoncée
/// à −50 dB et ne faisait aucun bruit, indiscernable d'un mute volontaire.
/// C'est le même défaut que #2886 avait corrigé dans la colonne `zones.volume`,
/// resté entier sur le fil.
///
/// Le message NOMME la grille et le plancher : sans le chiffre, le client ne
/// peut ni corriger sa demande ni construire un champ de saisie qui l'évite.
pub fn refus_de_resolution(
    resolution: crate::outputs::VolumeResolution,
    db: f64,
) -> Option<String> {
    let linear = db_to_linear(db)?;
    if resolution.holds(linear) {
        return None;
    }
    // Seule une grille linéaire peut avaler un niveau audible ; `holds` l'a
    // déjà établi, ce `else` n'existe que pour lire `steps`.
    let crate::outputs::VolumeResolution::Linear { steps } = resolution else {
        return None;
    };
    let plancher = resolution.floor_db()?;
    Some(format!(
        "{db:.1} dB est plus bas que ce que cette sortie sait tenir : elle ne reçoit qu'un \
         entier sur {steps} pas, et son plus petit niveau audible vaut {plancher:.1} dB. \
         Au-dessous, la valeur envoyée s'arrondit à zéro — la zone ne baisserait pas, elle \
         se tairait."
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tolérance de comparaison en dB. Volontairement mille fois plus fine que
    /// le « dB près » que réclame l'issue : ce qu'on vérifie ici, c'est que la
    /// conversion est exacte, pas qu'elle est suffisante.
    const EPS_DB: f64 = 1e-9;

    #[test]
    fn bornes_de_l_echelle() {
        // Pleine échelle = 0 dB, exactement.
        assert_eq!(linear_to_db(1.0), Some(0.0));
        assert_eq!(db_to_linear(0.0), Some(1.0));
        // Le zéro est le silence, PAS un plancher chiffré : c'est la règle
        // qu'un plancher à -60 dB casserait, et un client qui renverrait ce
        // -60 rallumerait la zone.
        assert_eq!(linear_to_db(0.0), None);
        assert_eq!(db_to_linear(f64::NEG_INFINITY), Some(0.0));
    }

    #[test]
    fn mi_echelle_vaut_moins_six_db() {
        // Le malentendu que l'issue dénonce : 50 % n'est pas « moitié moins
        // fort », c'est -6,02 dB.
        let db = linear_to_db(0.5).expect("0.5 est audible");
        assert!((db - (-6.020_599_913_279_624)).abs() < EPS_DB, "{db}");
        // Et 90 %, que l'utilisateur croit à -10 dB, vaut -0,915 dB.
        let db90 = linear_to_db(0.9).expect("0.9 est audible");
        assert!((db90 - (-0.915_149_811_213_501_5)).abs() < EPS_DB, "{db90}");
    }

    #[test]
    fn decades_exactes() {
        for (linear, attendu) in [(0.1, -20.0), (0.01, -40.0), (0.001, -60.0)] {
            let db = linear_to_db(linear).expect("audible");
            assert!((db - attendu).abs() < 1e-12, "{linear} → {db}");
        }
    }

    #[test]
    fn reversible_lineaire_puis_db() {
        // Aller-retour dans le sens que lisent les clients : la valeur rendue
        // par l'API, renvoyée telle quelle, ne doit pas déplacer le volume.
        for i in 1..=1000 {
            let linear = f64::from(i) / 1000.0;
            let db = linear_to_db(linear).expect("audible");
            let retour = db_to_linear(db).expect("fini");
            assert!(
                (retour - linear).abs() < 1e-12,
                "{linear} → {db} dB → {retour}"
            );
        }
    }

    #[test]
    fn reversible_db_puis_lineaire() {
        // Aller-retour dans le sens qu'utilise un réglage au dB près, du
        // silence utile jusqu'à la pleine échelle, par pas de 0,1 dB.
        let mut db = -80.0;
        while db <= 0.0 {
            let linear = db_to_linear(db).expect("fini");
            let retour = linear_to_db(linear).expect("audible");
            assert!(
                (retour - db).abs() < EPS_DB,
                "{db} dB → {linear} → {retour}"
            );
            db += 0.1;
        }
    }

    #[test]
    fn le_plafond_est_l_unite() {
        // Demander +6 dB ne monte pas le son, ça écrête : on rend l'unité.
        assert_eq!(db_to_linear(6.0), Some(1.0));
        assert_eq!(db_to_linear(0.5), Some(1.0));
        // Symétriquement, un état interne débordé ne s'affiche pas en positif.
        assert_eq!(linear_to_db(1.5), Some(0.0));
    }

    #[test]
    fn entrees_aberrantes_refusees_sans_couper_le_son() {
        // NaN est un refus explicite, que la route doit traduire en 400.
        assert_eq!(db_to_linear(f64::NAN), None);
        // Un +inf reste borné au plafond plutôt que de rendre un infini.
        assert_eq!(db_to_linear(f64::INFINITY), Some(1.0));
        // Côté linéaire, tout ce qui n'est pas audible est du silence.
        assert_eq!(linear_to_db(-0.5), None);
        assert_eq!(linear_to_db(f64::NAN), None);
    }

    #[test]
    fn la_paire_json_est_toujours_coherente() {
        // Le champ additif ne remplace rien : `volume` reste EXACTEMENT ce
        // qu'il était, et `volume_db` en est la lecture, pas une autre mesure.
        let mut obj = serde_json::Map::new();
        inserer_volume(&mut obj, 0.5);
        assert_eq!(obj["volume"], serde_json::json!(0.5));
        let db = obj["volume_db"].as_f64().expect("un nombre");
        assert!((db - (-6.020_599_913_279_624)).abs() < EPS_DB, "{db}");

        // Silence : `volume` vaut toujours 0, et `volume_db` est `null` —
        // present, donc lisible, mais sans valeur inventée.
        let mut muet = serde_json::Map::new();
        inserer_volume(&mut muet, 0.0);
        assert_eq!(muet["volume"], serde_json::json!(0.0));
        assert_eq!(muet["volume_db"], serde_json::Value::Null);
        assert!(muet.contains_key("volume_db"), "le champ doit être présent");
    }

    #[test]
    fn demande_en_db_ou_en_lineaire_mais_jamais_les_deux() {
        // Le chemin que l'issue réclame : régler au dB près.
        let v = demande_lineaire(None, Some(-20.0)).expect("−20 dB est légitime");
        assert!((v - 0.1).abs() < 1e-12, "{v}");
        // Le chemin historique reste intact — c'est la rétro-compatibilité.
        assert_eq!(demande_lineaire(Some(0.42), None), Ok(0.42));
        // Les deux ensemble : refus explicite, aucun gagnant inventé.
        assert!(demande_lineaire(Some(0.5), Some(-6.0)).is_err());
        // Aucun des deux : refus, et surtout PAS un volume par défaut.
        assert!(demande_lineaire(None, None).is_err());
    }

    #[test]
    fn un_db_positif_est_refuse_pas_rabote() {
        // Rendre 1.0 en silence ferait croire au client qu'il a eu son +3 dB.
        assert!(demande_lineaire(None, Some(3.0)).is_err());
        // La pleine échelle, elle, est une demande valide.
        assert_eq!(demande_lineaire(None, Some(0.0)), Ok(1.0));
    }

    #[test]
    fn demande_lineaire_hors_bornes_ramenee_sans_paniquer() {
        // Le champ historique reste tolérant : les routes le bornaient déjà.
        assert_eq!(demande_lineaire(Some(1.7), None), Ok(1.0));
        assert_eq!(demande_lineaire(Some(-3.0), None), Ok(0.0));
    }

    #[test]
    fn le_reglage_au_db_pres_atteint_sa_cible() {
        // La promesse de l'issue, vérifiée d'un bout à l'autre de l'échelle
        // utile : demander −N dB et relire donne −N dB, au millième près.
        for n in 0..=60 {
            let cible = -f64::from(n);
            let lineaire = demande_lineaire(None, Some(cible)).expect("légitime");
            let relu = linear_to_db(lineaire).expect("audible");
            assert!(
                (relu - cible).abs() < 1e-9,
                "{cible} dB → {lineaire} → {relu}"
            );
        }
    }

    #[test]
    fn monotone_strictement_croissante() {
        // Une échelle de volume qui n'est pas monotone est un piège : deux
        // curseurs différents rendraient le même dB, ou pire, s'inverseraient.
        let mut precedent = f64::NEG_INFINITY;
        for i in 1..=1000 {
            let db = linear_to_db(f64::from(i) / 1000.0).expect("audible");
            assert!(db > precedent, "{i} : {db} <= {precedent}");
            precedent = db;
        }
    }

    /// #1274 — le refus nomme la grille ET le plancher, sinon il n'apprend
    /// rien à qui doit corriger sa demande.
    #[test]
    fn le_refus_nomme_la_grille_et_le_plancher() {
        use crate::outputs::VolumeResolution;
        let pour_cent = VolumeResolution::Linear { steps: 100 };
        let motif = refus_de_resolution(pour_cent, -50.0).expect("−50 dB est hors grille");
        assert!(motif.contains("-50.0 dB"), "{motif}");
        assert!(motif.contains("100 pas"), "{motif}");
        assert!(motif.contains("-40.0 dB"), "{motif}");
    }

    /// Et il se TAIT partout ailleurs : le garde-fou ne doit pas rendre le
    /// réglage en dB inutilisable là où le matériel suit.
    #[test]
    fn le_refus_se_tait_quand_la_sortie_suit() {
        use crate::outputs::VolumeResolution;
        let pour_cent = VolumeResolution::Linear { steps: 100 };
        // La cible de l'issue, et le plancher lui-même.
        for db in [-18.0, -20.0, -40.0, -6.0, 0.0] {
            assert_eq!(refus_de_resolution(pour_cent, db), None, "{db} dB");
        }
        // Les grilles fines et le continu n'opposent jamais de refus.
        for resolution in [
            VolumeResolution::Continuous,
            VolumeResolution::Decibels { step_mdb: 100 },
            VolumeResolution::Linear { steps: 65536 },
        ] {
            assert_eq!(
                refus_de_resolution(resolution, -80.0),
                None,
                "{resolution:?}"
            );
        }
    }
}
