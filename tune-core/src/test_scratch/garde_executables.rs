use std::path::{Path, PathBuf};
use syn::visit::{self, Visit};
use syn::{Attribute, Expr, ExprCall, Item, Meta, Token, punctuated::Punctuated};

// Un cfg(all(test, unix)) implique test, un cfg(any(test, unix)) ne l'implique pas.
fn exige_test(meta: &Meta) -> bool {
    if meta.path().is_ident("test") {
        return true;
    }
    let Meta::List(liste) = meta else {
        return false;
    };
    let Ok(termes) = liste.parse_args_with(Punctuated::<Meta, Token![,]>::parse_terminated) else {
        return false;
    };
    if liste.path.is_ident("all") {
        termes.iter().any(exige_test)
    } else {
        liste.path.is_ident("any") && !termes.is_empty() && termes.iter().all(exige_test)
    }
}

fn est_test(attrs: &[Attribute]) -> bool {
    attrs.iter().any(|a| {
        a.path().segments.last().is_some_and(|s| s.ident == "test")
            || (a.path().is_ident("cfg") && a.parse_args::<Meta>().is_ok_and(|m| exige_test(&m)))
    })
}

fn appel(expr: &Expr) -> Option<&syn::Ident> {
    let Expr::Path(p) = expr else { return None };
    Some(&p.path.segments.last()?.ident)
}

fn chemin(expr: &Expr) -> &Expr {
    match expr {
        Expr::Reference(e) => chemin(&e.expr),
        Expr::Paren(e) => chemin(&e.expr),
        _ => expr,
    }
}

fn executable(expr: &Expr) -> bool {
    let Expr::Call(mode) = expr else { return false };
    let Some(Expr::Lit(lit)) = mode.args.first() else {
        return false;
    };
    let syn::Lit::Int(n) = &lit.lit else {
        return false;
    };
    appel(&mode.func).is_some_and(|id| id == "from_mode")
        && n.base10_parse::<u32>().is_ok_and(|mode| mode & 0o111 != 0)
}

#[derive(Default)]
struct Ecritures<'a> {
    chemins: Vec<&'a Expr>,
    violation: bool,
}

impl<'a> Visit<'a> for Ecritures<'a> {
    fn visit_item(&mut self, _: &'a Item) {} // une fonction imbriquée a sa propre portée

    fn visit_expr_call(&mut self, c: &'a ExprCall) {
        visit::visit_expr_call(self, c);
        let (Some(nom), Some(cible)) = (appel(&c.func), c.args.first()) else {
            return;
        };
        if nom == "write" {
            self.chemins.push(chemin(cible));
        } else if nom == "set_permissions"
            && self.chemins.contains(&chemin(cible))
            && c.args.get(1).is_some_and(executable)
        {
            self.violation = true;
        }
    }
}

struct Portee<'a> {
    fichier: &'a Path,
    dossier: PathBuf,
    test: bool,
    violations: &'a mut Vec<String>,
}

impl Portee<'_> {
    fn fonction(&mut self, attrs: &[Attribute], nom: &syn::Ident, bloc: &syn::Block) {
        if self.test || est_test(attrs) {
            let mut ecritures = Ecritures::default();
            ecritures.visit_block(bloc);
            if ecritures.violation {
                self.violations
                    .push(format!("{}::{nom}", self.fichier.display()));
            }
        }
    }
}

impl<'ast> Visit<'ast> for Portee<'_> {
    fn visit_item_fn(&mut self, f: &'ast syn::ItemFn) {
        self.fonction(&f.attrs, &f.sig.ident, &f.block);
        let avant = self.test;
        self.test |= est_test(&f.attrs);
        self.visit_block(&f.block);
        self.test = avant;
    }

    fn visit_impl_item_fn(&mut self, f: &'ast syn::ImplItemFn) {
        self.fonction(&f.attrs, &f.sig.ident, &f.block);
    }

    fn visit_item_impl(&mut self, i: &'ast syn::ItemImpl) {
        let avant = self.test;
        self.test |= est_test(&i.attrs);
        visit::visit_item_impl(self, i);
        self.test = avant;
    }

    fn visit_item_mod(&mut self, m: &'ast syn::ItemMod) {
        let dossier = self.dossier.join(m.ident.to_string());
        let test = self.test || est_test(&m.attrs);
        if let Some((_, items)) = &m.content {
            let mut portee = Portee {
                fichier: self.fichier,
                dossier,
                test,
                violations: self.violations,
            };
            for item in items {
                portee.visit_item(item);
            }
        } else {
            let explicite = m.attrs.iter().find_map(|a| {
                if !a.path().is_ident("path") {
                    return None;
                }
                let Meta::NameValue(v) = &a.meta else {
                    return None;
                };
                let Expr::Lit(e) = &v.value else { return None };
                let syn::Lit::Str(s) = &e.lit else {
                    return None;
                };
                Some(s.value())
            });
            let fichier = if let Some(nom) = explicite {
                self.fichier.parent().unwrap().join(nom)
            } else if dossier.with_extension("rs").is_file() {
                dossier.with_extension("rs")
            } else {
                dossier.join("mod.rs")
            };
            if fichier.is_file() {
                analyser(&fichier, test, self.violations);
            }
        }
    }
}

fn analyser(fichier: &Path, test: bool, violations: &mut Vec<String>) {
    let contenu = std::fs::read_to_string(fichier).unwrap();
    if !contenu.contains("set_permissions") && !contenu.contains("mod") {
        return;
    }
    let syntaxe =
        syn::parse_file(&contenu).unwrap_or_else(|e| panic!("{} : {e}", fichier.display()));
    let dossier = match fichier.file_stem().unwrap().to_str().unwrap() {
        "lib" | "main" | "mod" => fichier.parent().unwrap().to_owned(),
        _ => fichier.with_extension(""),
    };
    let mut portee = Portee {
        fichier,
        dossier,
        test: test || est_test(&syntaxe.attrs),
        violations,
    };
    portee.visit_file(&syntaxe);
}

#[test]
fn les_tests_ne_fabriquent_pas_un_executable_pendant_les_lancements() {
    let racine = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut violations = Vec::new();
    analyser(&racine.join("src/lib.rs"), false, &mut violations);
    for entree in walkdir::WalkDir::new(racine.join("tests")) {
        let entree = entree.unwrap();
        if entree.path().extension().is_some_and(|e| e == "rs") {
            analyser(entree.path(), true, &mut violations);
        }
    }
    assert!(
        violations.is_empty(),
        "Utiliser une fixture exécutable : {violations:?}"
    );
}

#[test]
fn seuls_les_appels_du_meme_banc_et_du_meme_chemin_comptent() {
    let dossier = tempfile::tempdir().unwrap();
    let source = dossier.path().join("lib.rs");
    let auxiliaire = dossier.path().join("auxiliaire.rs");
    std::fs::write(&source, r#"
        #[cfg(all(unix, test))] mod auxiliaire;
        fn production() { fs::write(&p, b""); fs::set_permissions(&p, Permissions::from_mode(0o755)); }
        #[test] fn distincts() { fs::write(&p, b""); fs::set_permissions(&q, Permissions::from_mode(0o755)); }
        #[test] fn ordinaire() { fs::write(&p, b""); fs::set_permissions(&p, Permissions::from_mode(0o600)); }
        #[test] fn texte() { let _ = "fs::write(p, b); fs::set_permissions(p, Permissions::from_mode(0o755))"; }
        #[test] fn separes() { fs::write(&p, b""); fn autre() { fs::set_permissions(&p, Permissions::from_mode(0o755)); } }
    "#).unwrap();
    std::fs::write(&auxiliaire, r#"
        fn helper() {
            std::fs::write(repertoire.join("outil"), b"");
            std::fs::set_permissions(&repertoire.join("outil"), std::fs::Permissions::from_mode(0o500));
        }
    "#).unwrap();
    let mut violations = Vec::new();
    analyser(&source, false, &mut violations);
    assert_eq!(violations, [format!("{}::helper", auxiliaire.display())]);
}
