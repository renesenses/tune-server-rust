use super::{OpenFailure, classify_open_failure};

/// The exact string Yacine's log carried, six times, with no other clue.
#[test]
fn pipewire_unreachable_is_classified_as_server_or_permission() {
    assert_eq!(
        classify_open_failure(
            "A backend-specific error has occurred: ALSA function 'snd_pcm_open' \
             failed with error 'Host is down (112)'"
        ),
        OpenFailure::ServerUnreachable
    );
}

/// The real cause on 8 Aug 2026: an account outside the `audio` group, on a
/// machine driven over SSH. It can surface as a plain permission error, so
/// that wording must land in the same arm.
#[test]
fn a_permission_error_lands_in_the_same_arm() {
    assert_eq!(
        classify_open_failure("snd_pcm_open failed with error 'Permission denied'"),
        OpenFailure::ServerUnreachable
    );
}

#[test]
fn a_vanished_device_is_classified_as_gone() {
    assert_eq!(
        classify_open_failure("ALSA function 'snd_pcm_open' failed with error 'No such device'"),
        OpenFailure::DeviceGone
    );
}

#[test]
fn an_exclusively_held_device_is_classified_as_busy() {
    assert_eq!(
        classify_open_failure("Device or resource busy"),
        OpenFailure::Busy
    );
}

#[test]
fn an_unrecognised_error_does_not_guess() {
    assert_eq!(
        classify_open_failure("something nobody has seen before"),
        OpenFailure::Unknown
    );
}

/// Both renderings must exist for every arm, and stay in their own
/// language: the log is ours, the toast is the listener's.
#[test]
fn every_cause_renders_for_both_audiences() {
    for c in [
        OpenFailure::ServerUnreachable,
        OpenFailure::DeviceGone,
        OpenFailure::Busy,
        OpenFailure::Unknown,
    ] {
        assert!(!c.log_hint().is_empty(), "{c:?} has no log hint");
        assert!(!c.user_message().is_empty(), "{c:?} has no user message");
        // The toast must say what to do, not merely restate the failure.
        let m = c.user_message();
        assert!(
            m.contains("Vérifiez")
                || m.contains("Choisissez")
                || m.contains("Fermez")
                || m.contains("choisissez"),
            "{c:?} user message gives no action: {m}"
        );
    }
}

/// The contract the poller relies on: a failure is delivered once. If it
/// stuck around, the very next track would be stopped by the previous
/// track's error — a far worse bug than the silence this fixes.
#[test]
fn a_failure_is_delivered_once_then_cleared() {
    use super::super::traits::OutputTarget;
    let out = super::LocalOutput::new("test-device".into());
    assert!(
        out.take_output_failure().is_none(),
        "clean output must report nothing"
    );

    *out.open_failure.lock().unwrap() = Some("boum".into());
    assert_eq!(out.take_output_failure().as_deref(), Some("boum"));
    assert!(
        out.take_output_failure().is_none(),
        "a failure must never be reported twice"
    );
}

/// The `audio` group is the lesson of 8 Aug 2026 — if this hint ever loses
/// it, the next person driving Tune over SSH starts the hunt from scratch.
#[test]
fn the_unreachable_hint_names_the_audio_group() {
    let h = OpenFailure::ServerUnreachable.log_hint();
    assert!(h.contains("audio"), "got: {h}");
    let m = OpenFailure::ServerUnreachable.user_message();
    assert!(m.contains("audio"), "got: {m}");
}
