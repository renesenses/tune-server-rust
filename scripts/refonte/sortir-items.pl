#!/usr/bin/env perl
# sortir-items.pl <fichier.rs> <module> <item1,item2,...>
# Déplace des ITEMS DE NIVEAU RACINE (fn, struct, enum, const, static, type, impl … for) d'un fichier
# vers <dossier>/<module>.rs, texte copié tel quel, attributs et doc compris, toutes les occurrences
# d'un nom (variantes #[cfg]). Les items sans visibilité deviennent `pub(super)`. Le parent reçoit
# `mod <module>;` et `pub use <module>::*;` à la place du premier item déplacé : chaque nom reste
# atteignable au même chemin, avec la visibilité qu'il avait (un glob n'élargit jamais).
use strict; use warnings;
my ($fichier, $module, $liste) = @ARGV;
die "usage: $0 <fichier.rs> <module> <item,...>\n" unless $liste;
my @voulus = do { my %v; grep { !$v{$_}++ } split /,/, $liste };
open my $fh, '<', $fichier or die "$fichier: $!"; my @l = <$fh>; close $fh;
my $re_item = qr/^(pub(?:\([a-z]+\))?\s+)?(?:async\s+|unsafe\s+|extern\s+"C"\s+)*(fn|struct|enum|const|static|type|union|trait)\s+([A-Za-z0-9_]+)\b/;
my @occ;
for my $nom (@voulus) {
    my @is = grep { $l[$_] =~ $re_item && $3 eq $nom } 0..$#l;
    # impl blocs de ce type : `impl X {` / `impl Tr for X {` en col 0
    push @is, grep { $l[$_] =~ /^impl(?:<[^>]*>)?\s+(?:[A-Za-z0-9_:<>, ]+\s+for\s+)?\Q$nom\E\b/ } 0..$#l;
    die "item $nom introuvable en colonne 0\n" unless @is;
    push @occ, map { [$nom, $_] } sort { $a <=> $b } @is;
}
my %plage; my %nom_de;
for my $o (@occ) {
    my ($nom, $i) = @$o; my $cle = "$nom#$i"; $nom_de{$cle} = $nom;
    my $d = $i; $d-- while $d > 0 && $l[$d-1] =~ /^(#\[|\/\/)/;
    my $f = $i;
    my $ligne = $l[$i];
    if ($ligne =~ /\{\s*$/ || ($ligne =~ /\{/ && $ligne !~ /\}\s*$/ && $ligne !~ /;\s*$/)) {
        $f++ while $f < $#l && $l[$f] !~ /^\}\s*$/;
    } elsif ($ligne =~ /;\s*$/ || $ligne =~ /\}\s*$/) {
        $f = $i;
    } else {
        # signature/valeur repliée : jusqu'à `{` de fin de ligne puis `^}`, ou jusqu'à `;`
        my $k = $i; my $bloc = 0;
        while ($k < $#l) { if ($l[$k] =~ /\{\s*$/) { $bloc = 1; last } if ($l[$k] =~ /;\s*$/) { last } $k++ }
        $f = $k;
        if ($bloc) { $f++ while $f < $#l && $l[$f] !~ /^\}\s*$/; }
    }
    $plage{$cle} = [$d, $f];
}
my %corps; my $premier;
for my $cle (sort { $plage{$b}[0] <=> $plage{$a}[0] } keys %plage) {
    my ($d, $f) = @{$plage{$cle}}; my $nom = $nom_de{$cle};
    my @m = @l[$d..$f];
    for (@m) { s/^((?:async\s+|unsafe\s+)*(?:fn|struct|enum|const|static|type|union|trait)\s+\Q$nom\E\b)/pub(super) $1/; s/^    ((?:async\s+|unsafe\s+)*fn [A-Za-z0-9_]+\s*[<(])/    pub(super) $1/ }
    # champs de struct sans visibilité : privés au module d'origine, ils le restent pour le parent et ses tests → pub(super)
    if (grep { /^(?:pub(?:\([a-z]+\))?\s+)?struct\s+\Q$nom\E\b/ } @m) { for (@m) { s/^    ([a-z_][A-Za-z0-9_]*)\s*:/    pub(super) $1:/ } }
    $corps{$cle} = [@m];
    my $n = $f - $d + 1; $n++ if $f+1 <= $#l && $l[$f+1] =~ /^\s*$/;
    splice @l, $d, $n; $premier = $d;
}
for (my $k = $#l; $k > 0; $k--) { splice @l, $k, 1 if $l[$k] =~ /^\s*$/ && $l[$k-1] =~ /^\s*$/; }
# `pub use` seulement si un item déplacé est pub ou pub(crate) : un glob qui ne réexporte rien est une erreur de compilation.
# Le glob prend la plus haute visibilité présente parmi les items déplacés : `pub` s'il y en a un `pub`,
# `pub(crate)` sinon, rien sinon — rustc refuse un glob qui ne réexporte rien à sa visibilité.
my $a_pub = grep { grep { /^pub\s/ } @$_ } values %corps;
my $a_crate = grep { grep { /^pub\(crate\)\s/ } @$_ } values %corps;
my $glob = $a_pub ? "pub use ${module}::*;\n" : ($a_crate ? "pub(crate) use ${module}::*;\n" : "use ${module}::*;\n");
splice @l, $premier, 0, "mod $module;\n", $glob, "\n";
(my $dossier = $fichier) =~ s/\.rs$//; mkdir $dossier unless -d $dossier;
my $cible = "$dossier/$module.rs"; die "$cible existe déjà\n" if -e $cible;
open my $out, '>', $cible or die; print $out "use super::*;\n\n";
my @ordre = sort { $plage{$a}[0] <=> $plage{$b}[0] } keys %plage;
for my $k (0..$#ordre) { print $out @{$corps{$ordre[$k]}}; print $out "\n" if $k < $#ordre; }
close $out; open my $w, '>', $fichier or die; print $w @l; close $w;
printf "%s : %d items → %s\n", $fichier, scalar @ordre, $cible;
