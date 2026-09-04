#!/usr/bin/env perl
# sortir-famille.pl <fichier.rs> <Type> <module> <fn1,fn2,...>
# Déplace des méthodes d'un bloc `impl Type {` (col 0) vers <dossier>/<module>.rs,
# dans un second bloc `impl Type { … }` précédé de `use super::*;`.
# Déplacement pur : le texte des méthodes est copié tel quel (même indentation) ;
# seule la visibilité `fn` → `pub(super) fn` est ajoutée quand il n'y en avait
# aucune, ce qui rend exactement l'ensemble de visibilité d'avant (le module
# parent et ses descendants). Le fichier parent reçoit `mod <module>;` juste
# après la fermeture du bloc impl.
use strict; use warnings;
my ($fichier, $type, $module, $liste) = @ARGV;
die "usage: $0 <fichier.rs> <Type> <module> <fn1,fn2,...>\n" unless $liste;
my @voulus = split /,/, $liste;
open my $fh, '<', $fichier or die "$fichier: $!"; my @l = <$fh>; close $fh;

# Bloc impl
my ($debut_impl, $fin_impl);
for my $i (0..$#l) {
    if (!defined $debut_impl && $l[$i] =~ /^impl(?:<[^>]*>)?\s+\Q$type\E\b[^{]*\{\s*$/) { $debut_impl = $i; next; }
    if (defined $debut_impl && !defined $fin_impl && $l[$i] =~ /^\}\s*$/) { $fin_impl = $i; last; }
}
die "bloc impl $type introuvable\n" unless defined $fin_impl;

# Localiser chaque méthode : [premier attribut/doc, dernière ligne]
my %plage; my %vis;
for my $nom (@voulus) {
    my $i;
    for my $k ($debut_impl+1 .. $fin_impl-1) {
        if ($l[$k] =~ /^    (?:pub(?:\([a-z]+\))?\s+)?(?:async\s+)?(?:unsafe\s+)?fn \Q$nom\E\s*[<(]/) { $i = $k; last; }
    }
    die "méthode $nom introuvable dans impl $type\n" unless defined $i;
    my $d = $i;
    $d-- while $d-1 > $debut_impl && $l[$d-1] =~ /^    (?:#\[|\/\/)/;
    my $f = $i;
    if ($l[$i] !~ /\{\s*$/ || $l[$i] =~ /\}\s*$/) {
        # signature repliée ou méthode sur une ligne
        if ($l[$i] =~ /\{.*\}\s*$/) { $f = $i; }
        else { $f++ while $f < $fin_impl && $l[$f] !~ /\{\s*$/; }
    }
    if ($l[$f] !~ /\}\s*$/ || $f == $i && $l[$i] !~ /\{.*\}\s*$/) {
        $f++ while $f < $fin_impl && $l[$f] !~ /^    \}\s*$/;
        die "fin de $nom introuvable\n" if $f >= $fin_impl;
    }
    $plage{$nom} = [$d, $f];
}

# Extraire (en ordre décroissant pour ne pas décaler les indices)
my %corps;
for my $nom (sort { $plage{$b}[0] <=> $plage{$a}[0] } keys %plage) {
    my ($d, $f) = @{$plage{$nom}};
    my @m = @l[$d..$f];
    # visibilité : fn nue → pub(super)
    for (@m) { s/^    ((?:async\s+)?(?:unsafe\s+)?fn \Q$nom\E\s*[<(])/    pub(super) $1/ }
    $corps{$nom} = [@m];
    my $n = $f - $d + 1;
    $n++ if $f+1 <= $#l && $l[$f+1] =~ /^\s*$/;   # avale la ligne vide qui suit
    splice @l, $d, $n;
}
# Nettoyer une éventuelle double ligne vide / ligne vide avant `}` (rustfmt le ferait)
for (my $k = $#l; $k > 0; $k--) { splice @l, $k, 1 if $l[$k] =~ /^\s*$/ && ($l[$k-1] =~ /^\s*$/ || ($k+1 <= $#l && $l[$k+1] =~ /^\}\s*$/)); }

# Recalculer la fin du bloc impl et insérer la déclaration du module
for my $i (0..$#l) { if ($l[$i] =~ /^impl(?:<[^>]*>)?\s+\Q$type\E\b[^{]*\{\s*$/) { $debut_impl = $i; last; } }
for my $i ($debut_impl+1..$#l) { if ($l[$i] =~ /^\}\s*$/) { $fin_impl = $i; last; } }
splice @l, $fin_impl+1, 0, "\n", "mod $module;\n";

# Écrire le module enfant dans l'ordre d'origine
(my $dossier = $fichier) =~ s/\.rs$//;
mkdir $dossier unless -d $dossier;
my $cible = "$dossier/$module.rs";
die "$cible existe déjà\n" if -e $cible;
open my $out, '>', $cible or die "$cible: $!";
print $out "use super::*;\n\nimpl $type {\n";
my @ordre = sort { $plage{$a}[0] <=> $plage{$b}[0] } keys %plage;
for my $k (0..$#ordre) { print $out @{$corps{$ordre[$k]}}; print $out "\n" if $k < $#ordre; }
print $out "}\n"; close $out;
open my $w, '>', $fichier or die; print $w @l; close $w;
printf "%s : %d méthodes → %s (%d lignes)\n", $fichier, scalar @ordre, $cible, scalar(@ordre) ? do { open my $c,'<',$cible; my @x=<$c>; scalar @x } : 0;
