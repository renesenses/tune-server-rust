//! Le registre des enceintes qui ont mené une poignée de main jusqu'au bout.
//!
//! Il existe pour une raison précise, et c'est la porte de sortie de S2-a :
//! **savoir à qui on parle, et le prouver.** Ce que dit une enceinte dans son
//! `client/hello` — ses rôles, ses codecs, ses fréquences — n'est connu qu'après
//! le chiffrement, et se perdrait dans le journal. Il est donc gardé ici, et
//! `GET /devices/sendspin` le rend visible.
//!
//! Ce n'est **pas** un registre de sorties : rien ici ne fabrique de zone, et
//! `playback_supported` reste faux. Un pair vu est un fait observé, pas une
//! sortie utilisable.
//!
//! Depuis le mode de transition (11/09/2026), il porte une seconde charge :
//! **dire par quel transport chaque session est passée**. Un registre qui
//! mélangerait les sessions Noise et les sessions en clair rendrait impossible
//! de répondre à « cette enceinte me parle-t-elle en chiffré ? », et c'est
//! aussi lui qui alimente la protection contre la rétrogradation
//! ([`deja_vu_chiffre`]).

use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

/// Nombre de pairs conservés. Au-delà, le plus ancien sort.
///
/// Borné parce que le registre vit aussi longtemps que le processus et qu'une
/// enceinte qui se reconnecte en boucle ne doit pas faire enfler la mémoire —
/// le dépôt a déjà payé ce genre de fuite (`project_memory_leak_chantier`).
pub const MAX_PAIRS: usize = 32;

/// Une enceinte vue, telle qu'elle s'est décrite.
#[derive(Debug, Clone, PartialEq)]
pub struct PairVu {
    /// Son identité.
    ///
    /// Sur une session **chiffrée**, c'est sa clé publique, et la poignée de
    /// main Noise l'a prouvée. Sur une session **en clair**, ce n'est qu'une
    /// chaîne qu'elle a écrite dans son `client/hello` : rien ne la vérifie.
    /// [`PairVu::chiffre`] est le champ qui départage les deux.
    pub client_id: String,
    /// La suite qu'elle a choisie — `None` sur une session en clair, où il
    /// n'y a rien à chiffrer donc aucune suite.
    pub suite: Option<String>,
    /// **Cette session est-elle passée par Noise ?**
    ///
    /// C'est le champ qui rend l'état réel lisible. Faux veut dire : trames en
    /// clair sur le réseau local, `client_id` non prouvé, et mode de transition
    /// armé (sans quoi la session n'aurait pas eu lieu).
    pub chiffre: bool,
    /// Le nom qu'elle donne dans `client/hello` — qui fait foi sur le TXT mDNS.
    pub nom: Option<String>,
    /// Ses rôles versionnés, tels qu'annoncés.
    pub roles: Vec<String>,
    /// Ses capacités de lecture, brutes.
    pub player_support: Option<Value>,
    /// Le `client/hello` entier, tel qu'il est arrivé.
    pub hello_brut: Value,
    /// Quand, en secondes depuis l'époque.
    pub vu_a: u64,
}

fn registre() -> &'static Mutex<Vec<PairVu>> {
    static REGISTRE: OnceLock<Mutex<Vec<PairVu>>> = OnceLock::new();
    REGISTRE.get_or_init(|| Mutex::new(Vec::new()))
}

/// Horodate en secondes depuis l'époque.
pub fn maintenant() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Range un pair. Une reconnexion du même `client_id` remplace l'entrée
/// existante plutôt que d'en empiler une seconde : l'identité est la clé.
pub fn enregistrer(pair: PairVu) {
    let Ok(mut pairs) = registre().lock() else {
        // Un verrou empoisonne ne doit pas faire tomber une connexion audio.
        return;
    };
    if let Some(place) = pairs.iter().position(|p| p.client_id == pair.client_id) {
        pairs[place] = pair;
        return;
    }
    if pairs.len() >= MAX_PAIRS {
        pairs.remove(0);
    }
    pairs.push(pair);
}

/// Les pairs vus, du plus ancien au plus récent.
pub fn pairs_vus() -> Vec<PairVu> {
    registre().lock().map(|p| p.clone()).unwrap_or_default()
}

/// Cette identité a-t-elle déjà mené une poignée de main **Noise** ?
///
/// C'est la protection contre la **rétrogradation**, et elle est copiée de
/// l'implémentation de référence : un pair qui a prouvé qu'il sait se connecter
/// chiffré ne doit jamais être admis en clair sous le même `client_id`. Sans
/// elle, le mode de transition offrirait à n'importe qui sur le réseau local le
/// moyen d'usurper une enceinte connue en écrivant simplement son identifiant
/// dans un `client/hello`.
///
/// **Ce que cette garde ne couvre pas** : la mémoire du registre s'arrête au
/// processus, et S2-a ne persiste aucune identité. Un redémarrage de Tune
/// rouvre donc la fenêtre jusqu'à la prochaine connexion chiffrée du pair. Le
/// magasin durable, c'est S2-b — l'implémentation de référence s'appuie, elle,
/// sur son `pairing_store`.
#[must_use]
pub fn deja_vu_chiffre(client_id: &str) -> bool {
    pairs_vus()
        .iter()
        .any(|p| p.chiffre && p.client_id == client_id)
}

/// La description JSON des pairs, pour `GET /devices/sendspin`.
pub fn decrire() -> Vec<Value> {
    pairs_vus()
        .into_iter()
        .map(|p| {
            json!({
                "client_id": p.client_id,
                "suite": p.suite,
                "name": p.nom,
                "supported_roles": p.roles,
                "player_support": p.player_support,
                "hello": p.hello_brut,
                "seen_at": p.vu_a,
                // La question a laquelle un testeur doit pouvoir repondre sans
                // lire le code : « cette session est-elle chiffree ? ». Les deux
                // formes, parce qu'un booleen se lit mal dans un journal et un
                // mot se filtre mal dans un tableau de bord.
                "encrypted": p.chiffre,
                "transport": if p.chiffre { "noise" } else { "clair" },
                // Chiffre, la PSK Sentinelle est publique : rien n'authentifie
                // ce pair. En clair, son `client_id` n'est meme plus qu'une
                // pretention. Dans les deux cas : faux. S2-b s'en charge.
                "authenticated": false,
                "playable": false,
            })
        })
        .collect()
}

/// Vide le registre. Réservé aux tests — un test qui laisse ses pairs derrière
/// lui fait passer le suivant pour vert.
#[cfg(test)]
pub fn vider() {
    if let Ok(mut pairs) = registre().lock() {
        pairs.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Sérialise les témoins du registre.
    ///
    /// `cargo test` lance les tests d'un même binaire sur plusieurs fils, et le
    /// registre est un état de PROCESSUS : sans ce verrou, `vider()` d'un
    /// témoin tombe au milieu du comptage d'un autre. Mesuré : 31 au lieu de
    /// 32, une fois sur quelques exécutions — le genre de rouge intermittent
    /// qu'on finit par croire dû à la charge.
    ///
    /// Le verrou est repris même empoisonné : un témoin qui a paniqué ne doit
    /// pas condamner les suivants à paniquer aussi, ce qui masquerait la
    /// panique d'origine.
    fn verrou_des_temoins() -> std::sync::MutexGuard<'static, ()> {
        static VERROU: OnceLock<Mutex<()>> = OnceLock::new();
        VERROU
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn pair(id: &str) -> PairVu {
        PairVu {
            client_id: id.to_string(),
            suite: Some("25519_ChaChaPoly_SHA256".into()),
            chiffre: true,
            nom: Some("Cuisine".into()),
            roles: vec!["player@v1".into()],
            player_support: None,
            hello_brut: json!({}),
            vu_a: maintenant(),
        }
    }

    /// Le même, mais passé par le mode de transition.
    fn pair_en_clair(id: &str) -> PairVu {
        PairVu {
            suite: None,
            chiffre: false,
            ..pair(id)
        }
    }

    #[test]
    fn une_reconnexion_remplace_le_pair_au_lieu_de_l_empiler() {
        let _verrou = verrou_des_temoins();
        vider();
        enregistrer(pair("cle-a"));
        let mut revenu = pair("cle-a");
        revenu.nom = Some("Salon".into());
        enregistrer(revenu);
        let vus = pairs_vus();
        assert_eq!(
            vus.len(),
            1,
            "l'identite est la cle : une seule entree par client_id"
        );
        assert_eq!(
            vus[0].nom.as_deref(),
            Some("Salon"),
            "c'est la DERNIERE description qui vaut"
        );
        vider();
    }

    #[test]
    fn le_registre_est_borne() {
        let _verrou = verrou_des_temoins();
        vider();
        for i in 0..(MAX_PAIRS + 5) {
            enregistrer(pair(&format!("cle-{i}")));
        }
        assert_eq!(
            pairs_vus().len(),
            MAX_PAIRS,
            "un pair qui se reconnecte en boucle ne doit pas faire enfler la memoire"
        );
        vider();
    }

    #[test]
    fn un_pair_vu_n_est_jamais_annonce_jouable_ni_authentifie() {
        let _verrou = verrou_des_temoins();
        vider();
        enregistrer(pair("cle-a"));
        let decrit = decrire();
        assert_eq!(decrit[0]["playable"], json!(false), "S2-a ne joue rien");
        assert_eq!(
            decrit[0]["authenticated"],
            json!(false),
            "la PSK Sentinelle est PUBLIQUE : elle n'authentifie personne"
        );
        vider();
    }

    #[test]
    fn une_session_en_clair_est_nommee_comme_telle() {
        // Exigence n°2 du chantier : un testeur doit pouvoir repondre a
        // « cette session est-elle chiffree ? » sans lire le code.
        let _verrou = verrou_des_temoins();
        vider();
        enregistrer(pair("chiffre"));
        enregistrer(pair_en_clair("en-clair"));
        let decrit = decrire();

        let c = decrit
            .iter()
            .find(|d| d["client_id"] == json!("chiffre"))
            .expect("le pair chiffre");
        assert_eq!(c["encrypted"], json!(true));
        assert_eq!(c["transport"], json!("noise"));
        assert_eq!(c["suite"], json!("25519_ChaChaPoly_SHA256"));

        let e = decrit
            .iter()
            .find(|d| d["client_id"] == json!("en-clair"))
            .expect("le pair en clair");
        assert_eq!(e["encrypted"], json!(false));
        assert_eq!(
            e["transport"],
            json!("clair"),
            "une session non chiffree doit se NOMMER, pas se deviner"
        );
        assert_eq!(
            e["suite"],
            json!(null),
            "il n'y a aucune suite sur une session en clair : annoncer un nom \
             de suite ferait croire a du chiffrement"
        );
        assert_eq!(e["authenticated"], json!(false));
        vider();
    }

    #[test]
    fn la_protection_contre_la_retrogradation_ne_reconnait_que_le_chiffre() {
        let _verrou = verrou_des_temoins();
        vider();
        enregistrer(pair("a-prouve-noise"));
        enregistrer(pair_en_clair("n-a-rien-prouve"));
        assert!(
            deja_vu_chiffre("a-prouve-noise"),
            "un pair vu en Noise doit interdire sa propre rétrogradation"
        );
        assert!(
            !deja_vu_chiffre("n-a-rien-prouve"),
            "une session EN CLAIR ne prouve rien : elle ne doit pas suffire a \
             verrouiller une identite, sinon n'importe qui verrouillerait \
             n'importe quel nom"
        );
        assert!(!deja_vu_chiffre("jamais-vu"));
        vider();
    }
}
