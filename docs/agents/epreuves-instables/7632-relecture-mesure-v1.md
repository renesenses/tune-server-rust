# Relecture d’une mesure de calibration v1

Le témoin `anciennes_mesures_relues_sans_perte_et_positions_identiques` compare la correction calculée sur une mesure en mémoire à celle de la session importée depuis sa représentation JSON. La CI de #8912 a relevé des différences d’un ULP pour 19 valeurs sur 479, puis un écart de gain de 0,125 dB pour une tolérance de 0,1 dB.

Le greffon active `serde_json/float_roundtrip` pour que le décodeur retrouve les mêmes valeurs binaires après l’export. Le témoin compare les bits de chaque valeur de la courbe avant de vérifier les bandes et leurs gains. Il exerce aussi l’import à une position, l’export v2 et la moyenne de cinq positions identiques.

Pour la contre-épreuve, retirer `float_roundtrip` de la dépendance du greffon puis lancer le test ci-dessous. Le diagnostic de relecture indique l’indice de la première valeur différente. Restaurer l’option et exécuter vingt fois le témoin compilé sous charge.

```sh
cargo test -p tune-calibration-greffon --test mesure_simulee anciennes_mesures_relues_sans_perte_et_positions_identiques -- --exact --nocapture
```
