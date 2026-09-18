# #2369 — fin de flux DoP : disponibilité et erreurs terminales

JP Robbe / OpenAI Codex / jp-robbe-20260917-p1suite2-2369.

Base : 73707a08c1658289913058a9843250622be08521.
Worktree Shrek : /srv/builds/worktrees/jp-2369-20260917-suite2.
Target dédié : /srv/cache/tune/targets/jp-2369-20260917-suite2.
Lot : batch/jp-p1-audio-transport-20260917, même base.

## Ce qui est démontré et ce qui ne l'est pas

Les PR #3797 et #3869 sont fusionnées et leurs commits de fusion
5508d59cbc1edbf4dca2bbdd59efa78b99e5ba23 et
cf817df055dfc8cbe01fe2834c049408fd8a4bbd sont ancêtres de la base.

Le porteur DoP, les marqueurs et le diagnostic du rappel pilote ne sont
pas modifiés. Ce correctif ne résout ni n'explique le bruit blanc entendu
sur SMSL SU-1/SU-8, et ne livre pas de sortie DSD native.

Un défaut indépendant reste dans decode_dsd_to_dop_streaming :

- le dernier bloc court ignorait le résultat d'envoi au canal ;
- s'il constituait le premier bloc audio, first_chunk_sent et data_ready
  n'étaient jamais annoncés ;
- un canal fermé ou un timeout sur un bloc complet retournait aussi Ok,
  donc le producteur appelant pouvait écrire dsd_dop_stream_complete
  alors que le flux avait été interrompu.

Chaque bloc passe désormais par le même envoi : annonce de disponibilité
après succès seulement ; erreur nommée en cas de canal fermé ou timeout.
Un succès signifie que les octets ont été acceptés dans le canal, pas qu’ils
ont été joués par un DAC. La limite publique reste SEND_TIMEOUT_SECS = 300. Le corps interne reçoit
cette durée explicitement, ce qui permet d'exercer la même branche de timeout
en 20 ms dans les tests sans attendre cinq minutes.

## Conséquence d'un arrêt volontaire

Fermer le consommateur lors d'un Stop provoque également
dop_stream_consumer_closed. Le résultat signifie « flux interrompu » et
non « panne matérielle » : le décodeur ne connaît pas le motif de fermeture.

L'unique appelant de production, dans orchestrator/resolve_local.rs, traite
Err uniquement avec un warn dsd_dop_stream_failed ; il ne déclenche ici ni
événement fatal, ni relance, ni changement d'état de zone. Ce fichier n'est
pas modifié. Un Stop attendu peut donc ajouter cette ligne d'abandon
au lieu du faux message de fin complète : limite explicitement assumée.

Cet appelant notifie déjà la disponibilité après l'en-tête WAV et passe
data_ready=None au décodeur. Le témoin de notification couvre le contrat
de l'API ; il ne prouve pas qu'un blocage de démarrage terrain est réparé.

## Témoins

Quatre tests enregistrés depuis decode.rs, chacun parcourant DSF et DFF :

1. Les fixtures synthétiques existantes contiennent 9000 octets DSD/canal :
   4500 trames DoP stéréo, soit 27000 octets. Tout tient dans le bloc final
   de 65536 octets visé : il doit être livré, armer first_chunk_sent et
   notifier data_ready. Les marqueurs alternés sont vérifiés.
2. Un découpage en blocs alignés de 4092 octets plus fin courte rend les
   mêmes octets que le bloc unique. Un flux déjà annoncé ne notifie pas à
   nouveau. Ce témoin positif doit rester vert lors de la contre-épreuve.
3. Récepteur fermé avant le décodage, blocs complets et bloc final :
   erreur dop_stream_consumer_closed, aucune disponibilité annoncée.
   Cela couvre aussi le contrat d'annulation par fermeture de canal.
4. Récepteur vivant mais canal saturé, mêmes deux tailles de blocs :
   erreur dop_stream_send_timeout, aucun octet audio livré ni signal
   de disponibilité. Le contenu préchargé dans le canal reste intact.

Les sources sont les fixtures du dépôt, écrites dans des fichiers temporaires
puis lues par les parseurs DSF/DFF de production. Aucun renderer, DAC,
compte externe ou émission sonore. Le témoin de porteur existant #3797
reste la preuve indépendante de son contenu ; aucune refonte de l'encodeur.

## Validation Shrek

    export TUNE_TARGET_KEY=jp-2369-20260917-suite2 CARGO_BUILD_JOBS=6
    . /srv/cache/tune/env.sh
    cargo test -p tune-core --lib dop_terminal_tests_2369 \
      --no-default-features --features oaat

Résultats : quatre témoins nouveaux verts ; après restauration, 63 tests
sélectionnés par le filtre dop verts (ce filtre inclut aussi des noms
contenant adoption). Les tests du porteur et de l'encodeur existants sont
présents dans ce périmètre. Aucun test sous feature local-audio n'est
revendiqué : le moteur local réservé n'a pas été compilé/exercé ici.

    cargo test -p tune-core --lib dop_terminal_tests_2369 \
      --no-default-features --features oaat -- --test-threads=2
    cargo test -p tune-core --lib dop \
      --no-default-features --features oaat -- --test-threads=2

Contre-épreuve : seul le corps du décodeur est remis à celui de la base,
en conservant la durée injectable (300 s publique / 20 ms dans le témoin).
Les tests et le helper ajouté restent inchangés et compilent. Trois échecs
comportementaux, un témoin positif vert, code 101 :

- abandoned_consumer_is_not_success_for_full_or_short_block :
  « an abandoned DoP consumer is an interrupted stream, not completion:
  (24, 176400) » ;
- short_first_and_final_block_is_delivered_and_announced_for_dsf_and_dff :
  « dsf: the short final block is also the first payload and must be announced » ;
- stalled_consumer_reports_timeout_for_full_or_short_block :
  « timeout must not be reported as a completed DoP stream: (24, 176400) ».

complete_chunks_and_tail_match_single_block_without_duplicate_notification
reste vert. Restauration par copie (cp) ; hashes SHA-256 du décodeur et des
tests identiques aux sauvegardes ; les 63 tests redeviennent verts.
La fermeture après un premier bloc déjà accepté n'a pas de témoin dédié.

Formatage cargo fmt --all -- --check vert ; Clippy -D clippy::correctness
vert avec 434 avertissements préexistants, aucun nouvel avertissement du
correctif. Compilation plafonnée à six jobs, tests restaurés à deux threads.

    cargo clippy -p tune-core --lib --no-default-features --features oaat \
      -- -D clippy::correctness
    cargo fmt --all -- --check
 Les journaux bruts sont conservés
dans /srv/builds/jp-evidence/jp-2369-20260917-suite2/.

Cette PR utilise Refs #2369 : le ticket matériel demeure ouvert.
