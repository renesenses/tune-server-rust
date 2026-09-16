//! Request-language formatting for lost playback sessions (#4193).
pub(super) fn lost_session(
    lang: &str,
    title: &str,
    position_ms: Option<u64>,
    cause: Option<&str>,
) -> String {
    let key = if position_ms.is_some() {
        "playback.sessionLostAtPosition"
    } else {
        "playback.sessionLostBrowser"
    };
    let minutes = (tune_core::http::streamer::SESSION_IDLE_TIMEOUT.as_secs() / 60).to_string();
    let position = position_ms.map(|ms| format!("{}:{:02}", ms / 60_000, ms / 1000 % 60));
    // Interpolate the template in one pass: a title containing "{position}"
    // must remain literal rather than being processed as a second template.
    let template = crate::i18n::t(lang, key);
    let mut result = String::new();
    let mut rest = template.as_str();
    while let Some(start) = rest.find('{') {
        result.push_str(&rest[..start]);
        let Some(end) = rest[start..].find('}') else {
            result.push_str(&rest[start..]);
            rest = "";
            break;
        };
        let token = &rest[start..=start + end];
        result.push_str(match token {
            "{title}" => title,
            "{position}" => position.as_deref().unwrap_or(""),
            "{minutes}" => &minutes,
            _ => token,
        });
        rest = &rest[start + end + 1..];
    }
    result.push_str(rest);
    if let Some(cause) = cause {
        result.push(' ');
        result
            .push_str(&crate::i18n::t(lang, "playback.sessionLostCause").replace("{cause}", cause));
    }
    result
}
