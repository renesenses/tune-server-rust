use serde::{Deserialize, Serialize};

use crate::db::sqlite::SqliteDb;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SmartCollection {
    pub id: Option<i64>,
    pub name: String,
    pub rules: Vec<Rule>,
    pub match_mode: MatchMode,
    pub sort_by: Option<String>,
    pub sort_order: SortOrder,
    pub limit: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MatchMode {
    All,
    Any,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SortOrder {
    Asc,
    Desc,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Rule {
    pub field: String,
    pub operator: Operator,
    pub value: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Operator {
    Equals,
    NotEquals,
    Contains,
    NotContains,
    StartsWith,
    EndsWith,
    GreaterThan,
    LessThan,
    Between,
    IsEmpty,
    IsNotEmpty,
}

impl SmartCollection {
    pub fn compile_sql(&self) -> (String, Vec<String>) {
        let mut conditions = Vec::new();
        let mut params = Vec::new();

        for rule in &self.rules {
            let (cond, rule_params) = compile_rule(rule);
            conditions.push(cond);
            params.extend(rule_params);
        }

        let join = match self.match_mode {
            MatchMode::All => " AND ",
            MatchMode::Any => " OR ",
        };

        let where_clause = if conditions.is_empty() {
            "1=1".to_string()
        } else {
            conditions.join(join)
        };

        let mut sql = format!(
            "SELECT t.id FROM tracks t \
             LEFT JOIN albums al ON t.album_id = al.id \
             LEFT JOIN artists ar ON t.artist_id = ar.id \
             WHERE {where_clause}"
        );

        if let Some(ref sort) = self.sort_by {
            let col = field_to_column(sort);
            let order = match self.sort_order {
                SortOrder::Asc => "ASC",
                SortOrder::Desc => "DESC",
            };
            sql.push_str(&format!(" ORDER BY {col} {order}"));
        }

        if let Some(limit) = self.limit {
            sql.push_str(&format!(" LIMIT {limit}"));
        }

        (sql, params)
    }
}

fn field_to_column(field: &str) -> &str {
    match field {
        "title" => "t.title",
        "artist" => "ar.name",
        "album" => "al.title",
        "genre" => "t.genre",
        "year" => "al.year",
        "duration" => "t.duration_ms",
        "rating" => "t.rating",
        "play_count" => "t.play_count",
        "added_at" => "t.created_at",
        "track_number" => "t.track_number",
        "disc_number" => "t.disc_number",
        "sample_rate" => "t.sample_rate",
        "bit_depth" => "t.bit_depth",
        "format" => "t.format",
        "composer" => "t.composer",
        "bpm" => "t.bpm",
        _ => "t.title",
    }
}

fn compile_rule(rule: &Rule) -> (String, Vec<String>) {
    let col = field_to_column(&rule.field);
    match rule.operator {
        Operator::Equals => (format!("{col} = ?"), vec![rule.value.clone()]),
        Operator::NotEquals => (format!("{col} != ?"), vec![rule.value.clone()]),
        Operator::Contains => (
            format!("{col} LIKE ? COLLATE NOCASE"),
            vec![format!("%{}%", rule.value)],
        ),
        Operator::NotContains => (
            format!("{col} NOT LIKE ? COLLATE NOCASE"),
            vec![format!("%{}%", rule.value)],
        ),
        Operator::StartsWith => (
            format!("{col} LIKE ? COLLATE NOCASE"),
            vec![format!("{}%", rule.value)],
        ),
        Operator::EndsWith => (
            format!("{col} LIKE ? COLLATE NOCASE"),
            vec![format!("%{}", rule.value)],
        ),
        Operator::GreaterThan => (format!("{col} > ?"), vec![rule.value.clone()]),
        Operator::LessThan => (format!("{col} < ?"), vec![rule.value.clone()]),
        Operator::Between => {
            let parts: Vec<&str> = rule.value.splitn(2, ',').collect();
            if parts.len() == 2 {
                (
                    format!("{col} BETWEEN ? AND ?"),
                    vec![parts[0].trim().to_string(), parts[1].trim().to_string()],
                )
            } else {
                (format!("{col} = ?"), vec![rule.value.clone()])
            }
        }
        Operator::IsEmpty => (format!("({col} IS NULL OR {col} = '')"), vec![]),
        Operator::IsNotEmpty => (format!("({col} IS NOT NULL AND {col} != '')"), vec![]),
    }
}

pub struct SmartCollectionRepo {
    db: SqliteDb,
}

impl SmartCollectionRepo {
    pub fn new(db: SqliteDb) -> Self {
        Self { db }
    }

    pub fn save(&self, collection: &SmartCollection) -> Result<i64, String> {
        let rules_json = serde_json::to_string(&collection.rules).map_err(|e| e.to_string())?;
        let match_mode =
            serde_json::to_string(&collection.match_mode).map_err(|e| e.to_string())?;
        let sort_order =
            serde_json::to_string(&collection.sort_order).map_err(|e| e.to_string())?;

        let conn = self.db.connection().lock().unwrap();

        if let Some(id) = collection.id {
            conn.execute(
                "UPDATE smart_collections SET name=?, rules=?, match_mode=?, sort_by=?, sort_order=?, max_limit=? WHERE id=?",
                rusqlite::params![collection.name, rules_json, match_mode, collection.sort_by, sort_order, collection.limit, id],
            ).map_err(|e| e.to_string())?;
            Ok(id)
        } else {
            conn.execute(
                "INSERT INTO smart_collections (name, rules, match_mode, sort_by, sort_order, max_limit) VALUES (?, ?, ?, ?, ?, ?)",
                rusqlite::params![collection.name, rules_json, match_mode, collection.sort_by, sort_order, collection.limit],
            ).map_err(|e| e.to_string())?;
            Ok(conn.last_insert_rowid())
        }
    }

    pub fn get(&self, id: i64) -> Result<Option<SmartCollection>, String> {
        let conn = self.db.connection().lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT id, name, rules, match_mode, sort_by, sort_order, max_limit FROM smart_collections WHERE id = ?")
            .map_err(|e| e.to_string())?;

        stmt.query_row(rusqlite::params![id], |row| {
            let rules_json: String = row.get(2)?;
            let match_mode_json: String = row.get(3)?;
            let sort_order_json: String = row.get(5)?;

            Ok(SmartCollection {
                id: row.get(0).ok(),
                name: row.get(1)?,
                rules: serde_json::from_str(&rules_json).unwrap_or_default(),
                match_mode: serde_json::from_str(&match_mode_json).unwrap_or(MatchMode::All),
                sort_by: row.get(4).ok().flatten(),
                sort_order: serde_json::from_str(&sort_order_json).unwrap_or(SortOrder::Asc),
                limit: row.get(6).ok().flatten(),
            })
        })
        .optional()
        .map_err(|e| e.to_string())
    }

    pub fn list(&self) -> Result<Vec<SmartCollection>, String> {
        let conn = self.db.connection().lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT id, name, rules, match_mode, sort_by, sort_order, max_limit FROM smart_collections ORDER BY name")
            .map_err(|e| e.to_string())?;

        let collections = stmt
            .query_map([], |row| {
                let rules_json: String = row.get(2)?;
                let match_mode_json: String = row.get(3)?;
                let sort_order_json: String = row.get(5)?;

                Ok(SmartCollection {
                    id: row.get(0).ok(),
                    name: row.get(1)?,
                    rules: serde_json::from_str(&rules_json).unwrap_or_default(),
                    match_mode: serde_json::from_str(&match_mode_json).unwrap_or(MatchMode::All),
                    sort_by: row.get(4).ok().flatten(),
                    sort_order: serde_json::from_str(&sort_order_json).unwrap_or(SortOrder::Asc),
                    limit: row.get(6).ok().flatten(),
                })
            })
            .map_err(|e| e.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;

        Ok(collections)
    }

    pub fn delete(&self, id: i64) -> Result<(), String> {
        self.db
            .execute("DELETE FROM smart_collections WHERE id = ?", &[&id])?;
        Ok(())
    }

    pub fn execute_query(&self, collection: &SmartCollection) -> Result<Vec<i64>, String> {
        let (sql, params) = collection.compile_sql();
        let conn = self.db.connection().lock().unwrap();
        let mut stmt = conn.prepare(&sql).map_err(|e| e.to_string())?;

        let param_refs: Vec<&dyn rusqlite::types::ToSql> = params
            .iter()
            .map(|p| p as &dyn rusqlite::types::ToSql)
            .collect();

        let ids = stmt
            .query_map(param_refs.as_slice(), |row| row.get::<_, i64>(0))
            .map_err(|e| e.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;

        Ok(ids)
    }
}

use rusqlite::OptionalExtension;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compile_equals_rule() {
        let collection = SmartCollection {
            id: None,
            name: "Rock Tracks".into(),
            rules: vec![Rule {
                field: "genre".into(),
                operator: Operator::Equals,
                value: "Rock".into(),
            }],
            match_mode: MatchMode::All,
            sort_by: Some("title".into()),
            sort_order: SortOrder::Asc,
            limit: Some(100),
        };

        let (sql, params) = collection.compile_sql();
        assert!(sql.contains("t.genre = ?"));
        assert!(sql.contains("ORDER BY t.title ASC"));
        assert!(sql.contains("LIMIT 100"));
        assert_eq!(params, vec!["Rock"]);
    }

    #[test]
    fn compile_multiple_rules_all() {
        let collection = SmartCollection {
            id: None,
            name: "Test".into(),
            rules: vec![
                Rule {
                    field: "genre".into(),
                    operator: Operator::Contains,
                    value: "Jazz".into(),
                },
                Rule {
                    field: "year".into(),
                    operator: Operator::GreaterThan,
                    value: "2000".into(),
                },
            ],
            match_mode: MatchMode::All,
            sort_by: None,
            sort_order: SortOrder::Asc,
            limit: None,
        };

        let (sql, params) = collection.compile_sql();
        assert!(sql.contains(" AND "));
        assert!(sql.contains("t.genre LIKE ? COLLATE NOCASE"));
        assert!(sql.contains("al.year > ?"));
        assert_eq!(params.len(), 2);
    }

    #[test]
    fn compile_any_mode() {
        let collection = SmartCollection {
            id: None,
            name: "Mixed".into(),
            rules: vec![
                Rule {
                    field: "genre".into(),
                    operator: Operator::Equals,
                    value: "Rock".into(),
                },
                Rule {
                    field: "genre".into(),
                    operator: Operator::Equals,
                    value: "Pop".into(),
                },
            ],
            match_mode: MatchMode::Any,
            sort_by: None,
            sort_order: SortOrder::Asc,
            limit: None,
        };

        let (sql, _) = collection.compile_sql();
        assert!(sql.contains(" OR "));
    }

    #[test]
    fn compile_between() {
        let rule = Rule {
            field: "year".into(),
            operator: Operator::Between,
            value: "1990, 2000".into(),
        };
        let (cond, params) = compile_rule(&rule);
        assert!(cond.contains("BETWEEN"));
        assert_eq!(params, vec!["1990", "2000"]);
    }

    #[test]
    fn compile_is_empty() {
        let rule = Rule {
            field: "composer".into(),
            operator: Operator::IsEmpty,
            value: String::new(),
        };
        let (cond, params) = compile_rule(&rule);
        assert!(cond.contains("IS NULL"));
        assert!(params.is_empty());
    }

    #[test]
    fn empty_rules() {
        let collection = SmartCollection {
            id: None,
            name: "All".into(),
            rules: vec![],
            match_mode: MatchMode::All,
            sort_by: None,
            sort_order: SortOrder::Asc,
            limit: None,
        };

        let (sql, params) = collection.compile_sql();
        assert!(sql.contains("1=1"));
        assert!(params.is_empty());
    }

    #[test]
    fn field_mapping() {
        assert_eq!(field_to_column("title"), "t.title");
        assert_eq!(field_to_column("artist"), "ar.name");
        assert_eq!(field_to_column("year"), "al.year");
        assert_eq!(field_to_column("unknown"), "t.title");
    }
}
