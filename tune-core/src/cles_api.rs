//! Les CLÉS D'API PERSONNELLES de l'utilisateur : AcoustID et Discogs.
//!
//! Bertrand, 12/10/2026 (go du lot 1 « Enrichissement MetaRust ») : « le max,
//! plus l'usage avec clé d'API perso ». L'utilisateur saisit dans Réglages ▸
//! Clés d'API sa propre clé d'application AcoustID et son propre jeton
//! personnel Discogs. Sans clé personnelle, Tune garde la clé intégrée, si
//! elle existe (réglage historique `acoustid_api_key` / `discogs_token`,
//! `tune.toml`, variable d'environnement).
//!
//! L'idée de laisser chacun apporter sa clé vient de MetaRust, de Xavier Joly
//! (`ACOUSTID_API_KEY`, `DISCOGS_TOKEN`, saisie masquée dans son interface) ;
//! le stockage est réécrit pour Tune : en base, scellé par [`crate::coffre`],
//! jamais dans un fichier `.discogs_token` du répertoire courant.
//!
//! # Les quatre règles
//!
//! 1. **Chiffrée au repos** : la valeur est scellée ([`crate::coffre`]) sous un
//!    nom de réglage que [`crate::secrets::est_secret`] reconnaît (`api_key`,
//!    `token`) — elle est donc AUSSI masquée par `GET /system/config` et
//!    retirée des exports, sans rien ajouter à ces routes.
//! 2. **Jamais rendue en clair** : [`EtatCle`] ne porte que les quatre
//!    derniers caractères ([`crate::coffre::fin_de_cle`]).
//! 3. **Jamais journalisée** : [`CleResolue`] a un `Debug` caviardé, et aucune
//!    fonction d'ici ne trace la valeur.
//! 4. **La clé personnelle passe avant la clé intégrée** : [`resoudre`].

use std::fmt;

use serde::Serialize;

use crate::coffre::{Coffre, fin_de_cle};
use crate::db::settings_repo::SettingsRepo;

/// Les services dont l'utilisateur peut apporter sa clé.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Service {
    /// La clé d'APPLICATION AcoustID (`client=` de `/v2/lookup`).
    AcoustId,
    /// Le jeton PERSONNEL Discogs (`Authorization: Discogs token=…`).
    Discogs,
}

impl Service {
    pub const TOUS: [Service; 2] = [Service::AcoustId, Service::Discogs];

    /// L'identifiant d'URL : `/system/api-keys/{service}`.
    pub fn code(self) -> &'static str {
        match self {
            Service::AcoustId => "acoustid",
            Service::Discogs => "discogs",
        }
    }

    pub fn depuis_code(code: &str) -> Option<Service> {
        match code.trim().to_ascii_lowercase().as_str() {
            "acoustid" => Some(Service::AcoustId),
            "discogs" => Some(Service::Discogs),
            _ => None,
        }
    }

    /// Le réglage qui porte la valeur SCELLÉE. Le nom contient `api_key` ou
    /// `token` : [`crate::secrets::est_secret`] le masque et l'exclut des
    /// exports.
    pub fn reglage_scelle(self) -> &'static str {
        match self {
            Service::AcoustId => "acoustid_api_key_personnelle",
            Service::Discogs => "discogs_token_personnel",
        }
    }
}

/// D'où vient la clé utilisée.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Origine {
    /// Saisie par l'utilisateur dans Réglages ▸ Clés d'API.
    Personnelle,
    /// La clé du serveur : réglage historique, `tune.toml` ou environnement.
    Integree,
}

/// Une clé prête à l'emploi. Son `Debug` ne montre JAMAIS la valeur : un
/// `debug!(?cle)` oublié ne peut pas la faire fuir dans le journal.
#[derive(Clone, PartialEq, Eq)]
pub struct CleResolue {
    valeur: String,
    pub origine: Origine,
}

impl CleResolue {
    pub fn valeur(&self) -> &str {
        &self.valeur
    }
    pub fn into_valeur(self) -> String {
        self.valeur
    }
}

impl fmt::Debug for CleResolue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CleResolue")
            .field("valeur", &"********")
            .field("origine", &self.origine)
            .finish()
    }
}

/// Ce que l'interface a le droit de savoir d'une clé. Aucune valeur.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EtatCle {
    pub service: Service,
    /// Une clé personnelle est enregistrée.
    pub personnelle: bool,
    /// Ses quatre derniers caractères, quand elle s'ouvre (vide si elle est
    /// trop courte pour en montrer sans la révéler).
    pub fin: Option<String>,
    /// Elle est enregistrée mais ne s'ouvre plus (clé maîtresse absente :
    /// base déplacée sans son fichier) — à ressaisir.
    pub illisible: bool,
    /// Une clé intégrée existe, qui sert quand il n'y a pas de personnelle.
    pub integree: bool,
    /// La clé qui sert réellement, s'il y en a une.
    pub utilisee: Option<Origine>,
}

/// Valide une saisie : ni vide, ni trop longue, ni caractère de contrôle
/// (une clé se colle souvent avec un retour à la ligne, qu'on retire).
pub fn valider(valeur: &str) -> Result<String, &'static str> {
    let v = valeur.trim();
    if v.is_empty() {
        return Err("empty_key");
    }
    if v.chars().count() > 512 {
        return Err("key_too_long");
    }
    if v.chars().any(char::is_control) {
        return Err("invalid_characters");
    }
    Ok(v.to_string())
}

/// Enregistre la clé personnelle, scellée.
pub fn poser(
    settings: &SettingsRepo,
    coffre: &Coffre,
    service: Service,
    valeur: &str,
) -> Result<(), String> {
    let v = valider(valeur).map_err(str::to_string)?;
    let scelle = coffre.sceller(&v)?;
    settings.set(service.reglage_scelle(), &scelle)
}

/// Retire la clé personnelle : la clé intégrée reprend la main.
pub fn retirer(settings: &SettingsRepo, service: Service) -> Result<(), String> {
    settings.delete(service.reglage_scelle())
}

/// La clé personnelle en clair, pour l'APPEL au service seulement.
///
/// `Err` quand elle est enregistrée mais ne s'ouvre pas ; `Ok(None)` quand il
/// n'y en a pas.
pub fn lire_personnelle(
    settings: &SettingsRepo,
    coffre: &Coffre,
    service: Service,
) -> Result<Option<String>, String> {
    let Some(scelle) = settings
        .get(service.reglage_scelle())?
        .filter(|s| !s.trim().is_empty())
    else {
        return Ok(None);
    };
    coffre.ouvrir(&scelle).map(Some)
}

/// La clé à utiliser : la PERSONNELLE d'abord, sinon l'intégrée.
///
/// Une personnelle illisible ne bloque pas le service : la clé intégrée sert,
/// et l'écran des Clés d'API dit qu'il faut ressaisir.
pub fn resoudre(
    settings: &SettingsRepo,
    coffre: &Coffre,
    service: Service,
    integree: Option<String>,
) -> Option<CleResolue> {
    if let Ok(Some(v)) = lire_personnelle(settings, coffre, service)
        && !v.trim().is_empty()
    {
        return Some(CleResolue {
            valeur: v,
            origine: Origine::Personnelle,
        });
    }
    integree
        .map(|k| k.trim().to_string())
        .filter(|k| !k.is_empty())
        .map(|valeur| CleResolue {
            valeur,
            origine: Origine::Integree,
        })
}

/// La clé à utiliser par une passe de `tune-core` qui ne voit pas la
/// configuration du serveur : la personnelle (coffre du serveur), sinon
/// `integree`.
pub fn resoudre_pour_le_serveur(
    settings: &SettingsRepo,
    service: Service,
    integree: Option<String>,
) -> Option<CleResolue> {
    match Coffre::du_serveur() {
        Some(coffre) => resoudre(settings, &coffre, service, integree),
        None => integree
            .map(|k| k.trim().to_string())
            .filter(|k| !k.is_empty())
            .map(|valeur| CleResolue {
                valeur,
                origine: Origine::Integree,
            }),
    }
}

/// L'état d'une clé, pour l'écran. Ne rend aucune valeur.
pub fn etat(settings: &SettingsRepo, coffre: &Coffre, service: Service, integree: bool) -> EtatCle {
    let enregistree = settings
        .get(service.reglage_scelle())
        .ok()
        .flatten()
        .is_some_and(|s| !s.trim().is_empty());
    let (fin, illisible) = if enregistree {
        match lire_personnelle(settings, coffre, service) {
            Ok(Some(v)) => (Some(fin_de_cle(&v)), false),
            _ => (None, true),
        }
    } else {
        (None, false)
    };
    let utilisee = if enregistree && !illisible {
        Some(Origine::Personnelle)
    } else if integree {
        Some(Origine::Integree)
    } else {
        None
    };
    EtatCle {
        service,
        personnelle: enregistree,
        fin,
        illisible,
        integree,
        utilisee,
    }
}

// ── Les tests « Tester » ──────────────────────────────────────────────────────

/// L'enregistrement EXEMPLE de la documentation publique d'AcoustID
/// (`/v2/lookup?trackid=…`), le même que MetaRust (`VERIFY_TRACK_ID`) : ce
/// n'est pas un secret, c'est l'identifiant que la documentation donne.
pub const ACOUSTID_PISTE_EXEMPLE: &str = "9ff43b6a-4f16-427c-93c2-92307ca505e0";

/// Verdict d'un « Tester ».
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VerdictTest {
    /// Le service a accepté la clé.
    Valide,
    /// Le service a refusé la clé.
    Refusee,
    /// Le service n'a pas répondu ou a répondu autre chose : on ne conclut pas.
    Indetermine,
}

/// Lit la réponse d'AcoustID à un `lookup` de la piste exemple. AcoustID
/// rend `{"status":"ok"}` pour une clé valide, et
/// `{"status":"error","error":{"code":4,"message":"invalid API key"}}` sinon.
pub fn verdict_acoustid(statut_http: u16, corps: &serde_json::Value) -> VerdictTest {
    match corps.get("status").and_then(|s| s.as_str()) {
        Some("ok") => VerdictTest::Valide,
        Some("error") => {
            let code = corps.pointer("/error/code").and_then(|c| c.as_i64());
            // 4 = invalid API key, 5 = invalid user API key (documentation).
            if matches!(code, Some(4) | Some(5)) {
                VerdictTest::Refusee
            } else {
                VerdictTest::Indetermine
            }
        }
        _ if statut_http == 401 || statut_http == 403 => VerdictTest::Refusee,
        _ => VerdictTest::Indetermine,
    }
}

/// Lit la réponse de Discogs à `GET /oauth/identity` : 200 avec `username`
/// pour un jeton valide, 401 sinon.
pub fn verdict_discogs(statut_http: u16, corps: &serde_json::Value) -> VerdictTest {
    match statut_http {
        200 if corps.get("username").is_some() || corps.get("id").is_some() => VerdictTest::Valide,
        401 | 403 => VerdictTest::Refusee,
        _ => VerdictTest::Indetermine,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    const CLE: &str = "PERSO-acoustid-9f8e7d6c5b4a";

    fn reglages() -> SettingsRepo {
        let db = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        crate::db::migrations::run_migrations(&db).unwrap();
        SettingsRepo::new(db)
    }

    #[test]
    fn une_cle_personnelle_passe_avant_la_cle_integree() {
        let dir = tempfile::tempdir().unwrap();
        let coffre = Coffre::new(dir.path());
        let s = reglages();
        // Sans personnelle : l'intégrée sert.
        let r = resoudre(&s, &coffre, Service::AcoustId, Some("INTEGREE".into())).unwrap();
        assert_eq!(r.valeur(), "INTEGREE");
        assert_eq!(r.origine, Origine::Integree);
        // Avec : la personnelle passe devant.
        poser(&s, &coffre, Service::AcoustId, CLE).unwrap();
        let r = resoudre(&s, &coffre, Service::AcoustId, Some("INTEGREE".into())).unwrap();
        assert_eq!(r.valeur(), CLE);
        assert_eq!(r.origine, Origine::Personnelle);
        // Discogs n'en est pas affecté.
        let d = resoudre(&s, &coffre, Service::Discogs, Some("JETON".into())).unwrap();
        assert_eq!(d.origine, Origine::Integree);
        // Retirée : l'intégrée reprend la main.
        retirer(&s, Service::AcoustId).unwrap();
        let r = resoudre(&s, &coffre, Service::AcoustId, Some("INTEGREE".into())).unwrap();
        assert_eq!(r.origine, Origine::Integree);
    }

    #[test]
    fn la_cle_est_chiffree_en_base() {
        let dir = tempfile::tempdir().unwrap();
        let coffre = Coffre::new(dir.path());
        let s = reglages();
        poser(&s, &coffre, Service::Discogs, CLE).unwrap();
        let brut = s.get(Service::Discogs.reglage_scelle()).unwrap().unwrap();
        assert!(!brut.contains(CLE), "la base ne doit porter que le scellé");
        assert!(!brut.contains("9f8e7d6c5b4a"));
        // Le nom du réglage est reconnu comme secret par la liste unique.
        for svc in Service::TOUS {
            assert!(crate::secrets::est_secret(svc.reglage_scelle()));
        }
    }

    #[test]
    fn l_etat_ne_rend_que_les_quatre_derniers_caracteres() {
        let dir = tempfile::tempdir().unwrap();
        let coffre = Coffre::new(dir.path());
        let s = reglages();
        poser(&s, &coffre, Service::AcoustId, CLE).unwrap();
        let e = etat(&s, &coffre, Service::AcoustId, true);
        assert_eq!(e.fin.as_deref(), Some("5b4a"));
        assert_eq!(e.utilisee, Some(Origine::Personnelle));
        let json = serde_json::to_string(&e).unwrap();
        assert!(!json.contains(CLE), "{json}");
        assert!(!json.contains("9f8e7d6c"), "{json}");
    }

    #[test]
    fn une_cle_illisible_se_signale_et_laisse_servir_l_integree() {
        let dir = tempfile::tempdir().unwrap();
        let s = reglages();
        poser(&s, &Coffre::new(dir.path()), Service::AcoustId, CLE).unwrap();
        // La base change de machine sans son fichier de clé maîtresse.
        let ailleurs = tempfile::tempdir().unwrap();
        let autre = Coffre::new(ailleurs.path());
        let e = etat(&s, &autre, Service::AcoustId, true);
        assert!(e.illisible && e.personnelle);
        assert_eq!(e.utilisee, Some(Origine::Integree));
        let r = resoudre(&s, &autre, Service::AcoustId, Some("INTEGREE".into())).unwrap();
        assert_eq!(r.origine, Origine::Integree);
    }

    #[test]
    fn une_saisie_vide_ou_avec_controle_est_refusee() {
        assert_eq!(valider("  \n"), Err("empty_key"));
        assert_eq!(valider("ab\u{7}cd"), Err("invalid_characters"));
        assert_eq!(valider("  abc\n").unwrap(), "abc");
    }

    /// Le journal ne voit JAMAIS la clé : ni à l'enregistrement, ni à la
    /// résolution, ni par un `debug!(?cle)` oublié.
    #[test]
    fn une_cle_n_apparait_jamais_dans_le_journal() {
        #[derive(Clone, Default)]
        struct Journal(Arc<Mutex<Vec<u8>>>);
        impl std::io::Write for Journal {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Journal {
            type Writer = Journal;
            fn make_writer(&'a self) -> Self::Writer {
                self.clone()
            }
        }
        crate::journal_de_test::fiabiliser_la_capture();
        let journal = Journal::default();
        let abonne = tracing_subscriber::fmt()
            .with_writer(journal.clone())
            .with_ansi(false)
            .with_max_level(tracing::Level::TRACE)
            .finish();
        let dir = tempfile::tempdir().unwrap();
        let coffre = Coffre::new(dir.path());
        let s = reglages();
        tracing::subscriber::with_default(abonne, || {
            poser(&s, &coffre, Service::AcoustId, CLE).unwrap();
            let r = resoudre(&s, &coffre, Service::AcoustId, None).unwrap();
            tracing::info!(?r, "cle_resolue_temoin");
            let e = etat(&s, &coffre, Service::AcoustId, false);
            tracing::info!(?e, "etat_temoin");
        });
        let texte = String::from_utf8(journal.0.lock().unwrap().clone()).unwrap();
        assert!(
            texte.contains("cle_resolue_temoin"),
            "le témoin doit écrire : {texte}"
        );
        assert!(
            !texte.contains(CLE),
            "la clé a fui dans le journal : {texte}"
        );
        assert!(!texte.contains("9f8e7d6c"), "{texte}");
    }

    #[test]
    fn les_verdicts_de_test_lisent_les_reponses_reelles() {
        let ok = serde_json::json!({"status": "ok", "results": []});
        assert_eq!(verdict_acoustid(200, &ok), VerdictTest::Valide);
        let refus = serde_json::json!({"status": "error", "error": {"code": 4, "message": "invalid API key"}});
        assert_eq!(verdict_acoustid(400, &refus), VerdictTest::Refusee);
        let panne = serde_json::json!({"status": "error", "error": {"code": 14, "message": "service unavailable"}});
        assert_eq!(verdict_acoustid(503, &panne), VerdictTest::Indetermine);
        let ident = serde_json::json!({"id": 1, "username": "exemple", "resource_url": "https://api.discogs.com/users/exemple"});
        assert_eq!(verdict_discogs(200, &ident), VerdictTest::Valide);
        let non = serde_json::json!({"message": "You must authenticate to access this resource."});
        assert_eq!(verdict_discogs(401, &non), VerdictTest::Refusee);
        assert_eq!(
            verdict_discogs(502, &serde_json::json!({})),
            VerdictTest::Indetermine
        );
    }
}
