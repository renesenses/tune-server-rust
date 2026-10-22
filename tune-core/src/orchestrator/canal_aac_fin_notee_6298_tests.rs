//! #6298 (pissa7, fil 2261) — Tidal AAC sur WiiM : la suivante ne démarrait
//! pas. Le producteur du canal AAC → WAV lâchait son émetteur en fin de
//! décodage sans fermer l'entrée de la session : l'émetteur de réserve
//! (`_keep_alive_tx`) tenait le canal ouvert pour toujours, et le 404 de
//! #6050 n'était jamais rendu.

use super::*;

/// Un WAV de 0,2 s de silence, 44,1 kHz / 16 bits / stéréo.
fn ecrire_wav(chemin: &std::path::Path) {
    let octets_data: u32 = 44_100 * 4 / 5;
    let mut v: Vec<u8> = Vec::new();
    v.extend_from_slice(b"RIFF");
    v.extend_from_slice(&(36 + octets_data).to_le_bytes());
    v.extend_from_slice(b"WAVEfmt ");
    v.extend_from_slice(&16u32.to_le_bytes());
    v.extend_from_slice(&1u16.to_le_bytes());
    v.extend_from_slice(&2u16.to_le_bytes());
    v.extend_from_slice(&44_100u32.to_le_bytes());
    v.extend_from_slice(&(44_100u32 * 4).to_le_bytes());
    v.extend_from_slice(&4u16.to_le_bytes());
    v.extend_from_slice(&16u16.to_le_bytes());
    v.extend_from_slice(b"data");
    v.extend_from_slice(&octets_data.to_le_bytes());
    v.extend(std::iter::repeat_n(0u8, octets_data as usize));
    std::fs::write(chemin, v).unwrap();
}

#[tokio::test]
async fn la_fin_du_decodage_ferme_l_entree_de_la_session_6298() {
    let dossier = crate::test_scratch::scratch_dir("tune-6298-canal-aac");
    let fichier = dossier.join("piste.aac.wav");
    ecrire_wav(&fichier);

    let streamer = Arc::new(AudioStreamer::new(0));
    let info = StreamInfo {
        format: "wav".into(),
        mime_type: "audio/wav".into(),
        sample_rate: 44_100,
        bit_depth: 16,
        channels: 2,
        ..Default::default()
    };
    let (session_id, tx, data_ready) = streamer.create_session(info, false, 256).await;
    let session = streamer
        .sessions_state()
        .lock()
        .await
        .get(&session_id)
        .cloned()
        .expect("la session vient d'être créée");
    assert!(
        session.channel_fill().await.is_some(),
        "prémisse : le canal est ouvert pendant le décodage"
    );

    produire_le_canal_aac(CanalAac {
        streamer: streamer.clone(),
        session_id: session_id.clone(),
        fichier: fichier.to_string_lossy().into_owned(),
        cadence: 44_100,
        profondeur: 16,
        tx,
        data_ready,
        levels_tx: tokio::sync::mpsc::unbounded_channel().0,
        err_bus: None,
        zone_id: 14,
    })
    .await;

    assert!(
        session.channel_fill().await.is_none(),
        "#6298 : le décodage AAC est fini, mais l'émetteur de réserve tient \
         encore le canal : `fin_du_canal()` ne sera jamais notée et le renderer \
         qui relit `bytes=0-` attend 15 s au lieu du 404 de #6050"
    );
    assert!(!fichier.exists(), "le fichier téléchargé est supprimé");
}
