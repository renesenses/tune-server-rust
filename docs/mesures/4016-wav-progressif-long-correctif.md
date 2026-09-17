# #4016 — lecture du WAV progressif au-delà de 2 et 4 Gio

JP Robbe / OpenAI Codex / jp-robbe-20260917-4016-long-stream

Base : `1802035811196c9712186735c4ec809d8ffe2b12` (main/v0.9.152).
Branche : `fix/jp-robbe-20260917-4016-wav`.
Lot : `batch/jp-long-wav-20260917` ; RC d'intégration à désigner par le contrôleur.

## Défaut et correction

Le diagnostic #4338 a transporté toute la piste synthétique de 46 minutes,
384 kHz, 24 bits, stéréo : 6 359 040 000 octets de PCM. Le conteneur annonçait
pourtant 2 147 483 611 octets, soit environ 15 min 32 s. #4081 avait déjà
corrigé le choix du pré-transcodage impossible ; ce correctif traite la
fausse fin du conteneur progressif qui subsistait.

Pour une durée connue dépassant la borne de compatibilité signée, les deux
champs de taille RIFF/data emploient désormais `0xFFFFFFFF`, convention de
longueur indéterminée déjà utilisée par les radios compatibles. HTTP conserve
sa longueur exacte en `u64` : **6 359 040 044 octets**, en-tête compris.

Le décodeur progressif peut fournir lui-même un en-tête sans durée. La route
corrige alors ses deux tailles avant sa mise en réserve pour les connexions
suivantes. Elle conserve le format annoncé par le producteur, les 44 octets
d'en-tête, le PCM, les offsets HTTP et la mise en phase des reprises.
La correction du producteur concerne le premier en-tête PCM canonique de
44 octets émis en un bloc par les décodeurs actuels.

Les pistes courtes, durées inconnues et radios bornées gardent leur contrat
antérieur. Aucun nouveau format RF64, aucune migration, aucune version changée.

## Preuves et périmètre

- Cinq tests `long_wav_4016` dans la cible unitaire de `tune-stream-http` :
  les deux origines d'en-tête, reconnexion, reprises à 44 octets et au-delà de
  2/4 Gio, conservation des flux courts, seuil de changement.
- Ces tests passent l'en-tête HTTP réellement produit à Symphonia 0.6.0,
  avec une source PCM virtuelle seekable. Le lecteur cherche puis décode
  après 2 Gio, après 4 Gio et à la dernière trame. Les deux derniers
  échantillons sont vérifiés. Ce test rapide n'est pas un transfert de 6 Go.
- Le banc explicite `lecture_long_wav_4016` réalise, lui, deux vrais transferts
  HTTP complets (avec/sans en-tête du producteur). Symphonia reçoit directement
  le corps HTTP par un canal borné, sans seek, puis décode tous ses paquets.
  Il exige 1 059 840 000 trames, le marqueur final et les deux derniers
  échantillons. Aucun fichier PCM de plusieurs Go n'est matérialisé.
  L'EOF signalé par Symphonia pour un conteneur indéterminé n'est accepté que
  si les compteurs et le marqueur prouvent la fin complète.
- La suite HTTP complète garde les contrats radio, sondes et reprises/DoP.
  La suite des constructeurs WAV garde notamment la compatibilité signée
  des durées inconnues et du live borné.

Commandes Shrek (clé `jp-4016-fix-20260917`, environnement chargé, 6 jobs) :

```sh
cargo test --locked -j 6 -p tune-stream-http
cargo test --locked -j 6 -p tune-core --lib --no-default-features audio::wav::tests
cargo run --locked -j 6 -p tune-stream-http --example lecture_long_wav_4016
cargo clippy --locked -j 6 -p tune-stream-http --all-targets -- -D clippy::correctness
cargo fmt --all --check
```

## Limites

Le PCM du banc est synthétique : il ne valide ni le fichier FLAC original,
ni le rendu sonore, ni le DSP, ni le DAC ou le renderer de Cyrille.
La convention de longueur indéterminée exige un lecteur qui la comprend ;
un lecteur RIFF limité à des tailles finies de 32 bits ne gagne pas une
capacité de 64 bits avec ce correctif. La compatibilité matérielle des longues
pistes reste à confirmer sur place, notamment pour les appareils qui imposent
des tailles signées positives.

La durée exacte connue continue de venir de StreamInfo ; ce correctif
n'invente pas la durée d'une piste dont les métadonnées sont absentes.
Le banc vise exactement les 46 minutes rapportées et n'atteste pas tous les
comportements de fin de paquet de tous les lecteurs. Il ne corrige pas les
autres questions de terrain de #4016 et ne ferme donc pas l'issue entière.

## Résultats exécutés sur Shrek, 17 septembre 2026

| En-tête | Octets HTTP | Trames décodées | Durée du banc |
|---|---:|---:|---:|
| HTTP | 6359040044 | 1059840000 | 213.05 s |
| Producteur | 6359040044 | 1059840000 | 208.99 s |

RSS maximal du processus du banc complet : **26680 Kio** (mesure GNU time, compilation exclue). Les deux derniers échantillons et le marqueur final ont été vérifiés dans les deux cas.

48 tests unitaires HTTP + 1 test d’intégration HTTP + 15 tests WAV = **64 tests réussis**. Clippy avec `-D clippy::correctness` et `cargo fmt --all --check` réussissent ; les avertissements préexistants hors périmètre ne sont pas assimilés à un nettoyage du dépôt.

### Contre-épreuve

Les deux branches de correction de production ont été neutralisées (`if false && ...` dans le constructeur et la normalisation HTTP), sans modifier les témoins. Commande :

```sh
cargo test --locked -j 6 -p tune-stream-http long_wav_4016
```

Résultat : code 101, **4 échecs comportementaux / 5 tests**. Les trois tests de lecture/reconnexion/reprise échouent sur :

```text
le WAV ne doit pas annoncer une fin fictive avant 46 minutes (#4016): SeekError(OutOfRange)
```

Le test du seuil retrouve `2147483611` à la place de `4294967295`. Le témoin des pistes courtes reste vert. Restauration par `cp` des deux sources, SHA-256 des témoins inchangés, comparaison des sources restaurées, puis suite HTTP complète à nouveau verte (49 tests) et formatage vert.

Preuves : `/srv/builds/jp-evidence/jp-4016-fix-20260917` (journaux, SHA-256, sources avant/après, contre-épreuve). La batterie GitHub complète est demandée avec `ci:full` ; ses résultats sont suivis dans la PR, séparément de ces mesures Shrek.
