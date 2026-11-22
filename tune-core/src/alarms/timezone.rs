//! #8838 — l’heure des réveils appartient au réglage explicite, pas au conteneur.
use crate::db::backend::DbBackend;
use chrono::{
    DateTime, Datelike, FixedOffset, Local, LocalResult, NaiveDate, NaiveDateTime, NaiveTime,
    TimeZone, Timelike, Utc,
};

pub const ALARM_TIMEZONE_KEY: &str = "alarms_timezone";

pub fn validate_alarm_timezone(zone: &str) -> Result<(), String> {
    if zone.is_empty() || jiff::tz::TimeZone::get(zone).is_ok() {
        Ok(())
    } else {
        Err(format!("alarms_timezone : fuseau IANA inconnu ({zone})"))
    }
}

pub fn read_alarm_timezone(db: &dyn DbBackend) -> Result<String, String> {
    let row = db.query_one(
        "SELECT value FROM settings WHERE key = 'alarms_timezone'",
        &[],
    )?;
    let zone = row
        .as_ref()
        .and_then(|r| r.first())
        .and_then(|v| v.as_string())
        .unwrap_or_default();
    if let Err(error) = validate_alarm_timezone(&zone) {
        tracing::warn!(timezone = %zone, error = %error, "alarm_timezone_invalid_fallback_system");
        return Ok(String::new());
    }
    Ok(zone)
}

pub fn alarm_local_time(
    db: &dyn DbBackend,
    instant: DateTime<Utc>,
) -> Result<DateTime<FixedOffset>, String> {
    local_time(&read_alarm_timezone(db)?, instant)
}

fn raw_occurrence(
    zone: &str,
    date: NaiveDate,
    time: NaiveTime,
) -> Result<LocalResult<DateTime<Utc>>, String> {
    let naive = NaiveDateTime::new(date, time);
    if zone.is_empty() {
        return Ok(Local
            .from_local_datetime(&naive)
            .map(|t| t.with_timezone(&Utc)));
    }
    let tz = jiff::tz::TimeZone::get(zone).map_err(|e| e.to_string())?;
    let civil = naive
        .to_string()
        .parse::<jiff::civil::DateTime>()
        .map_err(|e| e.to_string())?;
    let ambiguous = tz.to_ambiguous_timestamp(civil);
    let to_chrono = |ts: jiff::Timestamp| {
        DateTime::from_timestamp(ts.as_second(), ts.subsec_nanosecond() as u32)
            .ok_or_else(|| "alarms_timezone : instant hors limites".to_string())
    };
    use jiff::tz::AmbiguousOffset;
    Ok(match ambiguous.offset() {
        AmbiguousOffset::Gap { .. } => LocalResult::None,
        AmbiguousOffset::Fold { .. } => LocalResult::Ambiguous(
            to_chrono(ambiguous.earlier().map_err(|e| e.to_string())?)?,
            to_chrono(ambiguous.later().map_err(|e| e.to_string())?)?,
        ),
        AmbiguousOffset::Unambiguous { .. } => LocalResult::Single(to_chrono(
            ambiguous.unambiguous().map_err(|e| e.to_string())?,
        )?),
    })
}

/// Une heure effacée par un changement d'heure sonne à la première minute
/// valide du même jour, et non à l'heure décalée de la durée du saut.
fn occurrence(
    zone: &str,
    date: NaiveDate,
    time: NaiveTime,
) -> Result<LocalResult<DateTime<Utc>>, String> {
    let mut candidate = NaiveDateTime::new(date, time);
    loop {
        let result = raw_occurrence(zone, date, candidate.time())?;
        if !matches!(result, LocalResult::None) {
            return Ok(result);
        }
        let Some(next) = candidate.checked_add_signed(chrono::Duration::minutes(1)) else {
            return Ok(LocalResult::None);
        };
        // Un jour entièrement supprimé n'a aucune occurrence. Ne pas attribuer
        // au jour suivant les règles de jours/fériés du jour disparu.
        if next.date() != date {
            return Ok(LocalResult::None);
        }
        candidate = next;
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Decision {
    Wait,
    Ring,
    Skip(&'static str),
}

fn flag(alarm: &serde_json::Value, key: &str) -> bool {
    alarm[key]
        .as_bool()
        .unwrap_or_else(|| alarm[key].as_i64().unwrap_or(0) != 0)
}
fn alarm_time(alarm: &serde_json::Value) -> Option<NaiveTime> {
    // Les anciennes écritures peuvent porter des secondes : le planificateur
    // historique lit les deux premiers champs et sonne à la minute.
    let mut fields = alarm["time"].as_str()?.split(':');
    NaiveTime::from_hms_opt(
        fields.next()?.parse().ok()?,
        fields.next()?.parse().ok()?,
        0,
    )
}

/// Une décision pour l’occurrence du jour ; le planificateur garde les ids
/// consommés jusqu’au changement de date locale (y compris l’heure répétée).
pub(super) fn decision(
    alarm: &serde_json::Value,
    zone: &str,
    instant: DateTime<Utc>,
) -> Result<Decision, String> {
    let now = local_time(zone, instant)?;
    let Some(time) = alarm_time(alarm) else {
        return Ok(Decision::Wait);
    };
    let Some(candidate) = occurrence(zone, now.date_naive(), time)?.earliest() else {
        return Ok(Decision::Wait);
    };
    let effective = local_time(zone, candidate)?;
    if (now.hour(), now.minute()) != (effective.hour(), effective.minute()) {
        return Ok(Decision::Wait);
    }
    if !super::resolve_alarm_days(alarm).contains(&now.weekday().num_days_from_monday()) {
        return Ok(Decision::Skip("day"));
    }
    if !flag(alarm, "enabled") {
        return Ok(Decision::Skip("disabled"));
    }
    if flag(alarm, "skip_holidays") && super::is_french_holiday(now.year(), now.month(), now.day())
    {
        return Ok(Decision::Skip("holiday"));
    }
    Ok(Decision::Ring)
}

/// La première occurrence réelle, hors jours exclus. À l’automne, seule la
/// première occurrence de l’heure doublée est proposée.
pub fn next_alarm_at(
    alarm: &serde_json::Value,
    zone: &str,
    instant: DateTime<Utc>,
) -> Result<Option<String>, String> {
    let now = local_time(zone, instant)?;
    if !flag(alarm, "enabled") {
        return Ok(None);
    }
    let Some(time) = alarm_time(alarm) else {
        return Ok(None);
    };
    let days = super::resolve_alarm_days(alarm);
    for n in 0..15 {
        let Some(date) = now.date_naive().checked_add_days(chrono::Days::new(n)) else {
            break;
        };
        if !days.contains(&date.weekday().num_days_from_monday()) {
            continue;
        }
        if flag(alarm, "skip_holidays")
            && super::is_french_holiday(date.year(), date.month(), date.day())
        {
            continue;
        }
        if let Some(candidate) = occurrence(zone, date, time)?.earliest()
            && candidate > instant
        {
            return Ok(Some(local_time(zone, candidate)?.to_rfc3339()));
        }
    }
    Ok(None)
}

pub(super) fn local_time(
    zone: &str,
    instant: DateTime<Utc>,
) -> Result<DateTime<FixedOffset>, String> {
    if zone.is_empty() {
        return Ok(instant.with_timezone(&Local).fixed_offset());
    }
    let tz = jiff::tz::TimeZone::get(zone).map_err(|e| e.to_string())?;
    let timestamp =
        jiff::Timestamp::new(instant.timestamp(), instant.timestamp_subsec_nanos() as i32)
            .map_err(|e| e.to_string())?;
    let offset = FixedOffset::east_opt(tz.to_offset(timestamp).seconds())
        .ok_or_else(|| "alarms_timezone : décalage hors limites".to_string())?;
    Ok(instant.with_timezone(&offset))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Datelike, Timelike};
    fn instant(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    #[test]
    fn reveil_8838_paris_hiver_ete() {
        for (utc, hour) in [("2026-11-17T10:10:00Z", 11), ("2026-07-17T09:10:00Z", 11)] {
            let now = local_time("Europe/Paris", instant(utc)).unwrap();
            assert_eq!((now.hour(), now.minute()), (hour, 10));
            assert_eq!(
                local_time("UTC", instant(utc)).unwrap().hour(),
                instant(utc).hour()
            );
        }
    }
    #[test]
    fn reveil_8838_jour_ferie_selon_le_fuseau() {
        let now = local_time("Europe/Paris", instant("2026-07-13T22:30:00Z")).unwrap();
        assert_eq!((now.year(), now.month(), now.day()), (2026, 7, 14));
        assert!(super::super::is_french_holiday(
            now.year(),
            now.month(),
            now.day()
        ));
        assert_eq!(now.weekday(), chrono::Weekday::Tue);
    }
    #[test]
    fn reveil_8838_passages_ete_hiver() {
        let first = local_time("Europe/Paris", instant("2026-10-25T00:30:00Z")).unwrap();
        let second = local_time("Europe/Paris", instant("2026-10-25T01:30:00Z")).unwrap();
        assert_eq!(first.naive_local(), second.naive_local());
        assert_ne!(first.offset(), second.offset());
        let before = local_time("Europe/Paris", instant("2026-03-29T00:59:00Z")).unwrap();
        let after = local_time("Europe/Paris", instant("2026-03-29T01:00:00Z")).unwrap();
        assert_eq!((before.hour(), after.hour()), (1, 3));
    }
    #[test]
    fn reveil_8838_mode_historique_et_reglage_invalide() {
        let utc = instant("2026-11-17T10:10:00Z");
        assert_eq!(
            local_time("", utc).unwrap(),
            utc.with_timezone(&Local).fixed_offset()
        );
        assert!(validate_alarm_timezone("Europe/Paris").is_ok());
        assert!(validate_alarm_timezone("").is_ok());
        assert!(validate_alarm_timezone("Europe/Inconnue").is_err());
        assert!(local_time("Europe/Inconnue", utc).is_err());
    }
}

#[cfg(test)]
mod schedule_tests {
    use super::*;
    use serde_json::json;
    fn alarm(time: &str) -> serde_json::Value {
        json!({"time":time,"days_of_week":"1111111","enabled":1})
    }
    fn utc(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }
    #[test]
    fn reveil_8838_decision_hiver_ete_ne_suit_pas_utc() {
        for instant in ["2026-11-17T10:10:00Z", "2026-07-17T09:10:00Z"] {
            assert_eq!(
                decision(&alarm("11:10"), "Europe/Paris", utc(instant)).unwrap(),
                Decision::Ring
            );
            assert_eq!(
                decision(&alarm("11:10"), "UTC", utc(instant)).unwrap(),
                Decision::Wait
            );
        }
    }
    #[test]
    fn reveil_8838_horaires_historiques_avec_secondes() {
        for time in ["11:10", "11:10:00", "11:10:30"] {
            assert_eq!(
                decision(&alarm(time), "Europe/Paris", utc("2026-11-17T10:10:00Z")).unwrap(),
                Decision::Ring
            );
        }
    }
    #[test]
    fn reveil_8838_sauts_explicites() {
        let mut a = alarm("00:30");
        a["skip_holidays"] = json!(1);
        assert_eq!(
            decision(&a, "Europe/Paris", utc("2026-07-13T22:30:00Z")).unwrap(),
            Decision::Skip("holiday")
        );
        a["skip_holidays"] = json!(0);
        a["enabled"] = json!(0);
        assert_eq!(
            decision(&a, "Europe/Paris", utc("2026-07-13T22:30:00Z")).unwrap(),
            Decision::Skip("disabled")
        );
        a["enabled"] = json!(1);
        a["days_of_week"] = json!("1000000");
        assert_eq!(
            decision(&a, "Europe/Paris", utc("2026-07-13T22:30:00Z")).unwrap(),
            Decision::Skip("day")
        );
    }
    #[test]
    fn reveil_8838_heure_absente_sonne_a_la_premiere_minute_valide() {
        let a = alarm("02:30");
        assert_eq!(
            decision(&a, "Europe/Paris", utc("2026-03-29T01:00:00Z")).unwrap(),
            Decision::Ring
        );
        assert_eq!(
            next_alarm_at(&a, "Europe/Paris", utc("2026-03-29T00:00:00Z"))
                .unwrap()
                .unwrap(),
            "2026-03-29T03:00:00+02:00"
        );
        assert_eq!(
            decision(&a, "Europe/Paris", utc("2026-03-29T00:59:00Z")).unwrap(),
            Decision::Wait
        );
        assert_eq!(
            decision(&a, "Europe/Paris", utc("2026-03-29T01:30:00Z")).unwrap(),
            Decision::Wait
        );
    }
    #[test]
    fn reveil_8838_heure_repetee_ne_propose_pas_la_seconde_occurrence() {
        let a = alarm("02:30");
        assert_eq!(
            next_alarm_at(&a, "Europe/Paris", utc("2026-10-25T00:31:00Z"))
                .unwrap()
                .unwrap(),
            "2026-10-26T02:30:00+01:00"
        );
    }
}

#[cfg(test)]
mod database_tests {
    use super::*;
    use crate::db::{migrations, settings_repo::SettingsRepo, sqlite::SqliteDb};
    #[test]
    fn reveil_8838_fuseau_stocke_invalide_replie_sur_systeme() {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        migrations::run_migrations(&db).unwrap();
        let repo = SettingsRepo::new(db.clone());
        repo.set(ALARM_TIMEZONE_KEY, "Europe/Inconnue").unwrap();
        assert_eq!(
            read_alarm_timezone(&db).unwrap(),
            "",
            "un fuseau restauré illisible doit utiliser le système"
        );
        let instant: DateTime<Utc> = "2026-11-17T10:10:00Z".parse().unwrap();
        assert_eq!(
            alarm_local_time(&db, instant).unwrap(),
            instant.with_timezone(&Local).fixed_offset()
        );
        assert_eq!(
            repo.get(ALARM_TIMEZONE_KEY).unwrap().as_deref(),
            Some("Europe/Inconnue")
        );
        db.execute_batch("DROP TABLE settings").unwrap();
        assert!(
            read_alarm_timezone(&db).is_err(),
            "une erreur SQL ne doit pas devenir un repli de fuseau"
        );
    }
    #[test]
    fn reveil_8838_reglage_persistant_et_migration_sans_decalage() {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        migrations::run_migrations(&db).unwrap();
        let repo = SettingsRepo::new(db.clone());
        assert_eq!(repo.get(ALARM_TIMEZONE_KEY).unwrap().as_deref(), Some(""));
        db.execute_batch("INSERT INTO alarms (time) VALUES ('11:10');")
            .unwrap();
        repo.set(ALARM_TIMEZONE_KEY, "Europe/Paris").unwrap();
        db.execute_batch(include_str!("../../migrations/alarms_timezone.sql"))
            .unwrap();
        assert_eq!(read_alarm_timezone(&db).unwrap(), "Europe/Paris");
        let now = alarm_local_time(&db, "2026-11-17T10:10:00Z".parse().unwrap()).unwrap();
        assert_eq!(now.hour(), 11);
        assert_eq!(
            db.query_one("SELECT time FROM alarms", &[])
                .unwrap()
                .unwrap()[0]
                .as_str(),
            Some("11:10")
        );
    }
}
