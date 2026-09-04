#!/usr/bin/env perl
# REF-T — Sortir les modules de test inline d'un fichier Rust en modules enfants.
#
# Pour chaque bloc de niveau racine
#     #[cfg(test)]
#     mod nom {
#         …
#     }
# le corps part dans <dossier>/<nom>.rs (dédenté de 4 espaces) et le bloc est
# remplacé par `#[cfg(test)]\nmod nom;`. Les attributs et commentaires de doc
# qui précèdent `#[cfg(test)]` restent avec la déclaration. Le module enfant
# garde le même chemin (`fichier::nom::test`), donc la liste nominative des
# tests doit être IDENTIQUE avant et après : c'est ce que comparer.sh exige.
#
# Réadressage des gardes qui relisent un fichier par chemin relatif au source :
# `include_str!("x.rs")` et `#[path = "x.rs"]` gagnent un `../` (le fichier
# descend d'un niveau). `read_to_string("src/…")` est relatif à la caisse : il
# ne change pas.
#
# Usage : sortir-tests.pl <chemin/du/fichier.rs>
# Écrit <chemin/du/fichier>/<nom>.rs pour chaque module, réécrit le fichier,
# et imprime un bilan : modules sortis, lignes déplacées, gardes réadressées.
use strict;
use warnings;

# Option : --mods a,b,c sort les modules NOMMÉS (production comprise), au lieu
# des seuls blocs `#[cfg(test)]`. Le bloc doit rester `[pub(...)] mod nom {`
# en colonne 0, fermé par `}` en colonne 0 ; la visibilité est conservée.
my @voulus;
if (@ARGV && $ARGV[0] eq "--mods") { shift; @voulus = split /,/, shift; }
my $fichier = shift or die "usage : $0 [--mods a,b] <fichier.rs>\n";
(my $dossier = $fichier) =~ s/\.rs$// or die "pas un .rs : $fichier\n";
open my $in, '<', $fichier or die "$fichier : $!";
my @lignes = <$in>;
close $in;

mkdir $dossier unless -d $dossier;

my @sortie;
my ($modules, $deplacees, $gardes) = (0, 0, 0);
my $i = 0;
while ($i < @lignes) {
    my $l = $lignes[$i];
    # Un bloc commence par `#[cfg(test)]` en colonne 0, éventuellement suivi
    # d'autres attributs, puis `mod nom {` en colonne 0.
    my $mode_test = ($l =~ /^\#\[cfg\(test\)\]\s*$/);
    my $mode_nom  = (@voulus && $l =~ /^(pub(?:\([a-z]+\))?\s+)?mod\s+([A-Za-z0-9_]+)\s*\{\s*$/ && grep { $_ eq $2 } @voulus);
    if ($mode_test || $mode_nom) {
        my $j = $mode_test ? $i + 1 : $i;
        my @attrs;
        while ($mode_test && $j < @lignes && $lignes[$j] =~ /^\#\[/) { push @attrs, $lignes[$j]; $j++ }
        if ($j < @lignes && $lignes[$j] =~ /^(pub(?:\([a-z]+\))?\s+)?mod\s+([A-Za-z0-9_]+)\s*\{\s*$/ && ($mode_test || grep { $_ eq $2 } @voulus)) {
            my ($vis, $nom) = ($1 // '', $2);
            # Fin du bloc : première `}` en colonne 0 après l'ouverture.
            my $k = $j + 1;
            while ($k < @lignes && $lignes[$k] !~ /^\}\s*$/) { $k++ }
            die "bloc mod $nom non fermé dans $fichier\n" if $k >= @lignes;
            my @corps = @lignes[$j + 1 .. $k - 1];
            # Dédenter de 4 espaces (les lignes vides restent vides).
            for (@corps) { s/^ {4}// }
            # Réadresser les gardes relatives au fichier source : TOUT chemin
            # relatif gagne un niveau, y compris ceux qui commencent déjà par
            # `../` (une fixture en `../../tests/fixtures/x` devient
            # `../../../tests/fixtures/x`). Seuls les chemins absolus restent.
            # Un marqueur \x00 empêche de rejouer la substitution sur son
            # propre résultat ; il est retiré ensuite.
            for (@corps) {
                $gardes++ while s/include_str!\(\s*"(?!\/)([^"]+)"\s*\)/include_str!(\x00"..\/$1")/g;
                $gardes++ while s/\#\[path\s*=\s*"(?!\/)([^"]+)"\s*\]/#[path = \x00"..\/$1"]/g;
                s/\x00//g;
            }
            my $cible = "$dossier/$nom.rs";
            die "$cible existe déjà\n" if -e $cible;
            open my $out, '>', $cible or die "$cible : $!";
            print $out @corps;
            close $out;
            push @sortie, ($mode_test ? ($l, @attrs) : ()), "${vis}mod $nom;\n";
            $modules++;
            $deplacees += scalar @corps;
            $i = $k + 1;
            next;
        }
    }
    push @sortie, $l;
    $i++;
}

open my $out, '>', $fichier or die "$fichier : $!";
print $out @sortie;
close $out;

printf "%s : %d module(s) sorti(s) vers %s/, %d ligne(s) déplacée(s), %d garde(s) réadressée(s)\n",
    $fichier, $modules, $dossier, $deplacees, $gardes;
