use std::collections::HashMap;
use std::sync::Arc;

use serde::Serialize;

use crate::db::backend::DbBackend;

#[derive(Debug, Clone, Serialize)]
pub struct GenreNode {
    pub name: String,
    pub count: i64,
    pub children: Vec<GenreNode>,
}

const GENRE_HIERARCHY: &[(&str, &[&str])] = &[
    (
        "Rock",
        &[
            "Alternative Rock",
            "Indie Rock",
            "Progressive Rock",
            "Punk Rock",
            "Hard Rock",
            "Classic Rock",
            "Post-Rock",
            "Psychedelic Rock",
            "Garage Rock",
            "Grunge",
            "Shoegaze",
            "Stoner Rock",
            "Brit Pop",
        ],
    ),
    (
        "Metal",
        &[
            "Heavy Metal",
            "Death Metal",
            "Black Metal",
            "Doom Metal",
            "Thrash Metal",
            "Power Metal",
            "Progressive Metal",
            "Symphonic Metal",
            "Post-Metal",
        ],
    ),
    (
        "Jazz",
        &[
            "Bebop",
            "Cool Jazz",
            "Free Jazz",
            "Fusion",
            "Smooth Jazz",
            "Acid Jazz",
            "Latin Jazz",
            "Vocal Jazz",
            "Big Band",
            "Swing",
        ],
    ),
    (
        "Electronic",
        &[
            "House",
            "Techno",
            "Ambient",
            "Drum and Bass",
            "Dubstep",
            "Trance",
            "IDM",
            "Synth Pop",
            "Electro",
            "Downtempo",
            "Trip Hop",
            "Chillwave",
            "Minimal",
        ],
    ),
    (
        "Classical",
        &[
            "Baroque",
            "Romantic",
            "Contemporary Classical",
            "Opera",
            "Chamber Music",
            "Orchestral",
            "Choral",
            "Minimalism",
        ],
    ),
    (
        "Hip-Hop",
        &[
            "Rap",
            "Trap",
            "Boom Bap",
            "Conscious Hip-Hop",
            "Lo-Fi Hip-Hop",
            "Grime",
            "Drill",
        ],
    ),
    (
        "R&B",
        &[
            "Soul",
            "Neo-Soul",
            "Funk",
            "Contemporary R&B",
            "Motown",
            "Gospel",
        ],
    ),
    (
        "Pop",
        &[
            "Indie Pop",
            "Dream Pop",
            "Power Pop",
            "Dance Pop",
            "Electro Pop",
            "K-Pop",
            "J-Pop",
            "Chanson Française",
        ],
    ),
    (
        "Folk",
        &[
            "Indie Folk",
            "Acoustic",
            "Americana",
            "Celtic",
            "Singer-Songwriter",
            "Chanson",
        ],
    ),
    (
        "Blues",
        &[
            "Delta Blues",
            "Chicago Blues",
            "Electric Blues",
            "Blues Rock",
        ],
    ),
    (
        "Country",
        &["Alt-Country", "Bluegrass", "Outlaw Country", "Country Rock"],
    ),
    ("Reggae", &["Dub", "Dancehall", "Ska", "Roots Reggae"]),
    (
        "World",
        &[
            "Afrobeat",
            "Bossa Nova",
            "Flamenco",
            "Fado",
            "Arabic",
            "Indian Classical",
            "Highlife",
            "Cumbia",
            "Raï",
        ],
    ),
    (
        "Soundtrack",
        &["Film Score", "Video Game", "TV Series", "Musical"],
    ),
];

pub fn build_genre_tree(db: &Arc<dyn DbBackend>) -> Vec<GenreNode> {
    let counts = genre_counts(db);
    let mut tree = Vec::new();

    for (parent, children) in GENRE_HIERARCHY {
        let child_nodes: Vec<GenreNode> = children
            .iter()
            .filter_map(|&child| {
                let count = find_count(&counts, child);
                if count > 0 {
                    Some(GenreNode {
                        name: child.to_string(),
                        count,
                        children: vec![],
                    })
                } else {
                    None
                }
            })
            .collect();

        let parent_count = find_count(&counts, parent);
        let total = parent_count + child_nodes.iter().map(|c| c.count).sum::<i64>();

        if total > 0 {
            tree.push(GenreNode {
                name: parent.to_string(),
                count: total,
                children: child_nodes,
            });
        }
    }

    let categorized: std::collections::HashSet<String> = GENRE_HIERARCHY
        .iter()
        .flat_map(|(parent, children)| {
            std::iter::once(parent.to_lowercase()).chain(children.iter().map(|c| c.to_lowercase()))
        })
        .collect();

    let mut other_children = Vec::new();
    for (genre, count) in &counts {
        if !categorized.contains(&genre.to_lowercase()) && *count > 0 {
            other_children.push(GenreNode {
                name: genre.clone(),
                count: *count,
                children: vec![],
            });
        }
    }
    other_children.sort_by(|a, b| b.count.cmp(&a.count));

    if !other_children.is_empty() {
        let total: i64 = other_children.iter().map(|c| c.count).sum();
        tree.push(GenreNode {
            name: "Other".to_string(),
            count: total,
            children: other_children,
        });
    }

    tree.sort_by(|a, b| b.count.cmp(&a.count));
    tree
}

pub fn find_parent_genre(genre: &str) -> Option<&'static str> {
    let lower = genre.to_lowercase();
    for (parent, children) in GENRE_HIERARCHY {
        if parent.to_lowercase() == lower {
            return Some(parent);
        }
        for child in *children {
            if child.to_lowercase() == lower {
                return Some(parent);
            }
        }
    }
    None
}

fn genre_counts(db: &Arc<dyn DbBackend>) -> HashMap<String, i64> {
    let mut counts = HashMap::new();

    let raw_rows = match db.query_many(
        "SELECT genre, COUNT(*) FROM tracks WHERE genre IS NOT NULL AND genre != '' GROUP BY genre",
        &[],
    ) {
        Ok(r) => r,
        Err(_) => return counts,
    };

    for r in &raw_rows {
        let genres_str = r[0].as_string().unwrap_or_default();
        let count = r[1].as_i64().unwrap_or(0);
        for g in genres_str.split(&[';', ',', '/'][..]) {
            let g = g.trim();
            if !g.is_empty() {
                *counts.entry(g.to_string()).or_insert(0) += count;
            }
        }
    }

    counts
}

fn find_count(counts: &HashMap<String, i64>, genre: &str) -> i64 {
    let lower = genre.to_lowercase();
    counts
        .iter()
        .filter(|(k, _)| k.to_lowercase() == lower)
        .map(|(_, v)| *v)
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_parent_rock() {
        assert_eq!(find_parent_genre("Indie Rock"), Some("Rock"));
        assert_eq!(find_parent_genre("Grunge"), Some("Rock"));
    }

    #[test]
    fn find_parent_top_level() {
        assert_eq!(find_parent_genre("Jazz"), Some("Jazz"));
        assert_eq!(find_parent_genre("Electronic"), Some("Electronic"));
    }

    #[test]
    fn find_parent_unknown() {
        assert_eq!(find_parent_genre("Polka"), None);
    }

    #[test]
    fn find_parent_case_insensitive() {
        assert_eq!(find_parent_genre("indie rock"), Some("Rock"));
        assert_eq!(find_parent_genre("JAZZ"), Some("Jazz"));
    }

    #[test]
    fn genre_tree_empty_db() {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        let tree = build_genre_tree(&db);
        assert!(tree.is_empty());
    }

    #[test]
    fn hierarchy_no_duplicates() {
        let mut all_genres = Vec::new();
        for (parent, children) in GENRE_HIERARCHY {
            all_genres.push(parent.to_lowercase());
            for child in *children {
                all_genres.push(child.to_lowercase());
            }
        }
        let unique: std::collections::HashSet<_> = all_genres.iter().collect();
        assert_eq!(
            all_genres.len(),
            unique.len(),
            "duplicate genres in hierarchy"
        );
    }
}
