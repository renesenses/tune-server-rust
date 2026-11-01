//! Écriture de courbes en SVG et en CSV, sans dépendance. Juste ce qu'il faut
//! pour publier les mesures du banc dans la documentation.

use std::fmt::Write as _;
use std::io;
use std::path::Path;

pub struct Serie<'a> {
    pub nom: &'a str,
    pub points: &'a [(f64, f64)],
}

const COULEURS: [&str; 6] = ["#1f5fbf", "#c0392b", "#1e8449", "#8e44ad", "#d68910", "#17a589"];

/// Écrit une courbe. `x_log` passe l'abscisse en logarithme décimal (les
/// points d'abscisse ≤ 0 sont ignorés).
#[allow(clippy::too_many_arguments)]
pub fn courbe(
    chemin: &Path,
    titre: &str,
    x_label: &str,
    y_label: &str,
    series: &[Serie<'_>],
    x_log: bool,
    y_min: f64,
    y_max: f64,
) -> io::Result<()> {
    let (w, h) = (960.0, 540.0);
    let (ml, mr, mt, mb) = (70.0, 20.0, 40.0, 50.0);
    let (pw, ph) = (w - ml - mr, h - mt - mb);
    let fx = |x: f64| if x_log { x.log10() } else { x };
    let mut xmin = f64::INFINITY;
    let mut xmax = f64::NEG_INFINITY;
    for s in series {
        for &(x, _) in s.points {
            if x_log && x <= 0.0 {
                continue;
            }
            xmin = xmin.min(fx(x));
            xmax = xmax.max(fx(x));
        }
    }
    if !(xmin < xmax) {
        xmin = 0.0;
        xmax = 1.0;
    }
    let px = |x: f64| ml + (fx(x) - xmin) / (xmax - xmin) * pw;
    let py = |y: f64| mt + (y_max - y.clamp(y_min, y_max)) / (y_max - y_min) * ph;

    let mut s = String::new();
    let _ = writeln!(
        s,
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{w}\" height=\"{h}\" viewBox=\"0 0 {w} {h}\" font-family=\"sans-serif\" font-size=\"12\">"
    );
    let _ = writeln!(s, "<rect width=\"{w}\" height=\"{h}\" fill=\"white\"/>");
    let _ = writeln!(
        s,
        "<text x=\"{}\" y=\"24\" text-anchor=\"middle\" font-size=\"15\">{}</text>",
        w / 2.0,
        echapper(titre)
    );
    // Grille Y
    let ny = 10;
    for i in 0..=ny {
        let y = y_min + (y_max - y_min) * i as f64 / ny as f64;
        let yy = py(y);
        let _ = writeln!(
            s,
            "<line x1=\"{ml}\" y1=\"{yy:.1}\" x2=\"{:.1}\" y2=\"{yy:.1}\" stroke=\"#ddd\"/>",
            w - mr
        );
        let _ = writeln!(
            s,
            "<text x=\"{:.1}\" y=\"{:.1}\" text-anchor=\"end\">{}</text>",
            ml - 6.0,
            yy + 4.0,
            format_nombre(y)
        );
    }
    // Grille X
    let ticks: Vec<f64> = if x_log {
        let mut t = Vec::new();
        let d0 = xmin.floor() as i32;
        let d1 = xmax.ceil() as i32;
        for d in d0..=d1 {
            for m in [1.0, 2.0, 5.0] {
                let v = m * 10f64.powi(d);
                if v.log10() >= xmin - 1e-9 && v.log10() <= xmax + 1e-9 {
                    t.push(v);
                }
            }
        }
        t
    } else {
        (0..=8).map(|i| xmin + (xmax - xmin) * i as f64 / 8.0).collect()
    };
    for &t in &ticks {
        let xx = px(t);
        let _ = writeln!(
            s,
            "<line x1=\"{xx:.1}\" y1=\"{mt}\" x2=\"{xx:.1}\" y2=\"{:.1}\" stroke=\"#eee\"/>",
            h - mb
        );
        let _ = writeln!(
            s,
            "<text x=\"{xx:.1}\" y=\"{:.1}\" text-anchor=\"middle\">{}</text>",
            h - mb + 16.0,
            format_nombre(t)
        );
    }
    let _ = writeln!(
        s,
        "<rect x=\"{ml}\" y=\"{mt}\" width=\"{pw}\" height=\"{ph}\" fill=\"none\" stroke=\"#444\"/>"
    );
    let _ = writeln!(
        s,
        "<text x=\"{:.1}\" y=\"{:.1}\" text-anchor=\"middle\">{}</text>",
        ml + pw / 2.0,
        h - 8.0,
        echapper(x_label)
    );
    let _ = writeln!(
        s,
        "<text x=\"14\" y=\"{:.1}\" text-anchor=\"middle\" transform=\"rotate(-90 14 {:.1})\">{}</text>",
        mt + ph / 2.0,
        mt + ph / 2.0,
        echapper(y_label)
    );
    for (i, serie) in series.iter().enumerate() {
        let couleur = COULEURS[i % COULEURS.len()];
        let mut d = String::new();
        let mut premier = true;
        for &(x, y) in serie.points {
            if (x_log && x <= 0.0) || !y.is_finite() {
                continue;
            }
            let _ = write!(
                d,
                "{}{:.1},{:.1} ",
                if premier { "M" } else { "L" },
                px(x),
                py(y)
            );
            premier = false;
        }
        let _ = writeln!(
            s,
            "<path d=\"{d}\" fill=\"none\" stroke=\"{couleur}\" stroke-width=\"1.4\"/>"
        );
        let ly = mt + 14.0 + 16.0 * i as f64;
        let _ = writeln!(
            s,
            "<line x1=\"{:.1}\" y1=\"{ly:.1}\" x2=\"{:.1}\" y2=\"{ly:.1}\" stroke=\"{couleur}\" stroke-width=\"2\"/><text x=\"{:.1}\" y=\"{:.1}\">{}</text>",
            ml + 10.0,
            ml + 34.0,
            ml + 40.0,
            ly + 4.0,
            echapper(serie.nom)
        );
    }
    s.push_str("</svg>\n");
    std::fs::write(chemin, s)
}

/// Écrit des points en CSV (séparateur `;`, décimales avec un point).
pub fn csv(chemin: &Path, entete: &str, points: &[(f64, f64)]) -> io::Result<()> {
    let mut s = String::with_capacity(points.len() * 24);
    s.push_str(entete);
    s.push('\n');
    for &(x, y) in points {
        let _ = writeln!(s, "{x};{y}");
    }
    std::fs::write(chemin, s)
}

fn format_nombre(v: f64) -> String {
    if v.abs() >= 1000.0 && (v / 1000.0).fract().abs() < 1e-9 {
        format!("{}k", (v / 1000.0).round() as i64)
    } else if v.fract().abs() < 1e-9 {
        format!("{}", v.round() as i64)
    } else {
        format!("{v:.2}")
    }
}

fn echapper(t: &str) -> String {
    t.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}
