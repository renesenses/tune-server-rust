use super::retry_open;
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

/// Sortie mémoire uniquement : l'ouverture refuse tant que son horloge
/// factice n'a pas atteint la disponibilité du nouveau format.
#[derive(Default)]
struct FakeOutput {
    opens: Vec<(u32, u16)>,
    rendered: Vec<(u32, Vec<i32>)>,
    starts: usize,
}

struct FakeStream {
    output: Rc<RefCell<FakeOutput>>,
    sample_rate: u32,
}

impl FakeStream {
    fn play(&self, samples: &[i32]) {
        let mut output = self.output.borrow_mut();
        output.starts += 1;
        output.rendered.push((self.sample_rate, samples.to_vec()));
    }
}

fn open_track(
    output: &Rc<RefCell<FakeOutput>>,
    sample_rate: u32,
    available_after_ms: u64,
) -> (FakeStream, u64) {
    let clock = Cell::new(0);
    let stream = retry_open(
        true,
        || {
            output.borrow_mut().opens.push((sample_rate, 2));
            if clock.get() < available_after_ms {
                return Err("hardware temporarily unavailable");
            }
            Ok(FakeStream {
                output: output.clone(),
                sample_rate,
            })
        },
        |delay| {
            clock.set(clock.get() + delay.as_millis() as u64);
            true
        },
    )
    .expect("aucun arrêt demandé")
    .expect("le changement de fréquence doit reprendre seul, sans relance manuelle");
    (stream, clock.get())
}

#[test]
fn sample_rate_44100_to_96000_resumes_automatically_7207() {
    let output = Rc::new(RefCell::new(FakeOutput::default()));
    let track_1 = [i32::MIN, 0, i32::MAX, -1];
    let track_2 = [1, -2, 3, -4];
    let (first, elapsed) = open_track(&output, 44_100, 0);
    assert_eq!(elapsed, 0, "pas d'attente quand le périphérique accepte");
    first.play(&track_1);
    drop(first);

    // Une seule demande de lecture de la deuxième piste : aucun ré-appui.
    let (second, elapsed) = open_track(&output, 96_000, 3000);
    second.play(&track_2);
    assert_eq!(elapsed, 3000);
    let output = output.borrow();
    assert_eq!(output.starts, 2);
    assert_eq!(output.opens[0], (44_100, 2));
    assert_eq!(output.opens[1..], [(96_000, 2); 6]);
    assert_eq!(
        output.rendered,
        [(44_100, track_1.to_vec()), (96_000, track_2.to_vec())],
        "la reprise ne doit ni perdre, ni doubler, ni convertir les échantillons"
    );
}

#[test]
fn rate_changes_preserve_pending_blocks_7207() {
    let output = Rc::new(RefCell::new(FakeOutput::default()));
    let blocks = [
        (44_100, vec![7, -7]),
        (96_000, vec![9, -9]),
        (44_100, vec![11, -11]),
    ];
    for (index, (sample_rate, samples)) in blocks.iter().enumerate() {
        let (stream, _) = open_track(&output, *sample_rate, if index == 0 { 0 } else { 300 });
        stream.play(samples);
    }
    assert_eq!(output.borrow().rendered, blocks);
}

#[test]
fn dop_carrier_176400_preserves_samples_7207() {
    let output = Rc::new(RefCell::new(FakeOutput::default()));
    // Marqueurs DoP alternés, déjà alignés pour un conteneur i32.
    let samples = [
        0x05123400,
        0x05567800,
        0xfa123400u32 as i32,
        0xfa567800u32 as i32,
    ];
    let (stream, _) = open_track(&output, 176_400, 3000);
    stream.play(&samples);
    assert_eq!(output.borrow().rendered, [(176_400, samples.to_vec())]);
}

#[test]
fn persistent_failure_is_bounded_and_keeps_last_error_7207() {
    let mut attempts = 0;
    let mut elapsed = Duration::ZERO;
    let result: Option<Result<(), usize>> = retry_open(
        true,
        || {
            attempts += 1;
            Err(attempts)
        },
        |delay| {
            elapsed += delay;
            true
        },
    );
    assert_eq!(result, Some(Err(6)));
    assert_eq!(attempts, 6);
    assert_eq!(elapsed, Duration::from_secs(3));
}

#[test]
fn other_outputs_are_not_retried_7207() {
    let mut attempts = 0;
    let result: Option<Result<(), &str>> = retry_open(
        false,
        || {
            attempts += 1;
            Err("unsupported")
        },
        |delay| {
            assert!(delay.is_zero());
            true
        },
    );
    assert_eq!(result, Some(Err("unsupported")));
    assert_eq!(attempts, 1);
}

#[test]
fn stop_during_retry_does_not_reopen_or_report_failure_7207() {
    let mut attempts = 0;
    let result: Option<Result<(), &str>> = retry_open(
        true,
        || {
            attempts += 1;
            Err("busy")
        },
        |delay| delay.is_zero(),
    );
    assert_eq!(result, None);
    assert_eq!(attempts, 1, "pas de nouvelle ouverture après l'arrêt");
    let failure = std::sync::Mutex::new(None);
    super::super::RefusDOuverture::Cancelled.rapporter("sortie factice", &failure);
    assert_eq!(*failure.lock().unwrap(), None);
}

#[test]
fn stop_before_first_attempt_does_not_open_7207() {
    let result: Option<Result<(), &str>> = retry_open(
        true,
        || panic!("l'arrêt doit empêcher toute ouverture"),
        |_| false,
    );
    assert_eq!(result, None);
}

#[test]
fn only_resolved_alsa_hardware_is_retried_7207() {
    for (host, endpoint, expected) in [
        ("Alsa", "alsa:hw:CARD=1,DEV=0", true),
        ("Alsa", "alsa:hw:CARD=DAC,DEV=0", true),
        ("Alsa", "alsa:default", false),
        ("Alsa", "alsa:plughw:CARD=1,DEV=0", false),
        ("Alsa", "alsa:pipewire", false),
        ("Alsa", "", false),
        ("Wasapi", "alsa:hw:CARD=1,DEV=0", false),
    ] {
        assert_eq!(super::supports_open_retry(host, endpoint), expected);
    }
}
