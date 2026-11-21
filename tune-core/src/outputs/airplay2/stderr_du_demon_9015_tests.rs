//! #9015 — témoins du relais de la sortie d'erreur du démon.
//!
//! Contre-épreuves :
//! - `nettoyer_ligne` réduite à `Some(ligne.to_string())` fait tomber
//!   `aucun_secret_ne_passe_9015` ;
//! - `LIGNES_PAR_FENETRE` porté à `u32::MAX` fait tomber
//!   `un_demon_qui_boucle_n_inonde_pas_le_journal_9015` ;
//! - `.stderr(Stdio::null())` dans `commande_du_demon_avec` fait tomber
//!   `la_commande_du_demon_ouvre_sa_sortie_d_erreur_9015`.
use super::*;

async fn relaye(entree: &[u8]) -> Vec<String> {
    let mut lignes = Vec::new();
    relayer_vers(entree, |l| lignes.push(l)).await;
    lignes
}

#[tokio::test]
async fn les_lignes_de_diagnostic_passent_9015() {
    let lignes = relaye(
        b"WARN connect: transient failed: RTSP error: Setup failed: Cannot start playing from state Playing\n\n  \nINFO teardown sent\n",
    )
    .await;
    assert_eq!(
        lignes,
        vec![
            "WARN connect: transient failed: RTSP error: Setup failed: Cannot start playing from state Playing".to_string(),
            "INFO teardown sent".to_string(),
        ]
    );
}

#[tokio::test]
async fn aucun_secret_ne_passe_9015() {
    let entree = concat!(
        "pair-setup with pin=3939 refused\n",
        "connect password: hunter2 failed\n",
        "Authorization: Bearer eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiJ4In0.c2lnbmF0dXJl\n",
        "GET http://alice:motdepasse@192.168.1.37:7000/pair-pin-start\n",
        "ltpk 0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\n",
        "srp_proof=deadbeef secret_key=\"xyz\"\n",
        "token=abc123&x=1\n",
        "pair-verify failed; pin: Some(\"4242\"), identity=Some(abcd-7f)\n",
    );
    let lignes = relaye(entree.as_bytes()).await;
    let tout = lignes.join("\n");
    for secret in [
        "3939",
        "hunter2",
        "eyJhbGciOiJIUzI1NiJ9",
        "alice",
        "motdepasse",
        "0123456789abcdef0123456789abcdef",
        "deadbeef",
        "xyz",
        "abc123",
        "4242",
        "abcd-7f",
    ] {
        assert!(
            !tout.contains(secret),
            "secret « {secret} » relayé : {tout}"
        );
    }
    assert!(
        tout.contains("pair-setup with pin="),
        "le contexte reste : {tout}"
    );
    assert!(tout.contains("192.168.1.37"), "l'hôte reste : {tout}");
}

#[tokio::test]
async fn un_demon_qui_boucle_n_inonde_pas_le_journal_9015() {
    let mut entree = Vec::new();
    for i in 0..10_000 {
        entree.extend_from_slice(format!("WARN retry {i}\n").as_bytes());
    }
    let lignes = relaye(&entree).await;
    assert_eq!(
        lignes.len(),
        LIGNES_PAR_FENETRE as usize + 1,
        "{} lignes relayées",
        lignes.len()
    );
    assert_eq!(
        lignes.last().unwrap(),
        &format!("{} lignes tues (débit borné)", 10_000 - LIGNES_PAR_FENETRE)
    );
}

#[test]
fn une_fenetre_neuve_rend_la_parole_et_dit_les_lignes_tues_9015() {
    let t0 = Instant::now();
    let mut d = Debit::new(t0);
    for _ in 0..LIGNES_PAR_FENETRE {
        assert_eq!(d.ligne(t0), Verdict::Relayer { tues: 0 });
    }
    assert_eq!(d.ligne(t0), Verdict::Taire);
    assert_eq!(d.ligne(t0), Verdict::Taire);
    assert_eq!(d.ligne(t0 + FENETRE), Verdict::Relayer { tues: 2 });
    assert_eq!(d.reste(), 0);
}

#[tokio::test]
async fn une_ligne_geante_est_coupee_sans_etre_gardee_9015() {
    let mut entree = vec![b'a'; 0];
    entree.extend(std::iter::repeat_n(b'x', 5_000_000));
    entree.extend_from_slice(b"\nsuite\n");
    let lignes = relaye(&entree).await;
    assert_eq!(lignes.len(), 2);
    assert!(lignes[0].len() <= LONGUEUR_MAX);
    assert_eq!(lignes[1], "suite");
}

/// Le démon écrit sans fin et vite : le tube est vidé en continu, l'écrivain
/// n'est jamais bloqué même quand le journal se tait.
#[tokio::test]
async fn le_demon_n_est_jamais_bloque_par_son_tube_9015() {
    use tokio::io::AsyncWriteExt;
    let (mut ecrivain, lecteur) = tokio::io::duplex(1024);
    let relais = tokio::spawn(relayer_vers(lecteur, |_| {}));
    let ecriture = async {
        for i in 0..50_000 {
            ecrivain
                .write_all(format!("WARN boucle {i}\n").as_bytes())
                .await
                .unwrap();
        }
        drop(ecrivain);
    };
    tokio::time::timeout(Duration::from_secs(10), ecriture)
        .await
        .expect("le démon reste bloqué sur sa sortie d'erreur");
    tokio::time::timeout(Duration::from_secs(5), relais)
        .await
        .expect("le relais finit avec le flux")
        .unwrap();
}

/// Le câblage : la commande du démon ouvre bien un tube pour stderr.
#[cfg(unix)]
#[tokio::test]
async fn la_commande_du_demon_ouvre_sa_sortie_d_erreur_9015() {
    let mut commande = super::super::commande_du_demon_avec("/bin/sh", None);
    commande.arg("-c").arg("echo 'WARN pin=3939 refusé' >&2");
    let mut enfant = commande.spawn().expect("sh");
    let stderr = enfant
        .stderr
        .take()
        .expect("stderr du démon jeté : rien n'atteint le journal (#9015)");
    let lignes = {
        let mut l = Vec::new();
        relayer_vers(stderr, |x| l.push(x)).await;
        l
    };
    let _ = enfant.wait().await;
    assert_eq!(lignes, vec![format!("WARN pin={MASQUE} refusé")]);
}
