#![cfg(all(target_os = "linux", feature = "local-audio"))]
// These tests call CPAL's public API, therefore its actual worker/poll code.
// Only ALSA boundary responses for a private null PCM are fault-injected.
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::{
    ffi::CString,
    process::{Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};
fn until(message: &str, mut predicate: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(2);
    while !predicate() {
        assert!(Instant::now() < end, "{message}");
        std::thread::sleep(Duration::from_millis(2));
    }
}
fn subprocess(scenario: i32, name: &str) {
    let dir = tempfile::tempdir().unwrap();
    let shim = dir.path().join("faults.so");
    let source = dir.path().join("faults.c");
    std::fs::write(&source, include_str!("fixtures/alsa_4295/faults.c")).unwrap();
    let output = Command::new("cc")
        .args(["-shared", "-fPIC", "-std=c11", "-Werror", "-O1"])
        .arg(&source)
        .args(["-ldl", "-o"])
        .arg(&shim)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "shim compile: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let config = dir.path().join("alsa.conf");
    std::fs::write(&config, "pcm.!default { type null }\n").unwrap();
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture"])
        .env("TUNE_4295_CHILD", scenario.to_string())
        .env("ALSA_CONFIG_PATH", config)
        .env("LD_PRELOAD", shim)
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(
                status.success(),
                "real CPAL worker failed scenario {scenario}: {status}"
            );
            return;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("CPAL worker/drop exceeded 8 seconds, scenario {scenario}");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}
fn scenario(id: i32, name: &str) {
    if std::env::var("TUNE_4295_CHILD").ok().as_deref() != Some(&id.to_string()) {
        return subprocess(id, name);
    }
    // Symbols are resolved only inside this isolated LD_PRELOAD child.
    let arm: unsafe extern "C" fn(i32) = unsafe {
        let p = libc::dlsym(
            libc::RTLD_DEFAULT,
            CString::new("tune_4295_arm").unwrap().as_ptr(),
        );
        assert!(!p.is_null());
        std::mem::transmute(p)
    };
    let count: unsafe extern "C" fn(i32) -> i32 = unsafe {
        let p = libc::dlsym(
            libc::RTLD_DEFAULT,
            CString::new("tune_4295_count").unwrap().as_ptr(),
        );
        assert!(!p.is_null());
        std::mem::transmute(p)
    };
    let host = cpal::host_from_id(cpal::HostId::Alsa).unwrap();
    let device = host.default_output_device().unwrap();
    let callbacks = Arc::new(AtomicUsize::new(0));
    let underruns = Arc::new(AtomicUsize::new(0));
    let disconnected = Arc::new(AtomicUsize::new(0));
    let data = callbacks.clone();
    let underrun = underruns.clone();
    let lost = disconnected.clone();
    let stream = device
        .build_output_stream(
            &cpal::StreamConfig {
                channels: 2,
                sample_rate: 48000,
                buffer_size: cpal::BufferSize::Fixed(128),
            },
            move |samples: &mut [f32], _| {
                samples.fill(0.0);
                data.fetch_add(1, Ordering::SeqCst);
            },
            move |err| match err {
                cpal::StreamError::BufferUnderrun => {
                    underrun.fetch_add(1, Ordering::SeqCst);
                }
                cpal::StreamError::DeviceNotAvailable => {
                    lost.fetch_add(1, Ordering::SeqCst);
                }
                _ => {}
            },
            Some(Duration::from_millis(20)),
        )
        .unwrap();
    stream.play().unwrap();
    until("normal callback did not run", || {
        callbacks.load(Ordering::SeqCst) >= 3
    });
    unsafe {
        arm(id);
    }
    if matches!(id, 1..=3 | 8) {
        until(
            "POLLERR recovery was not attempted by the actual worker",
            || unsafe { count(2) > 0 && count(if id == 2 { 1 } else { 0 }) > 0 },
        );
        let after_recovery = callbacks.load(Ordering::SeqCst);
        until(
            "POLLERR recovery did not resume the actual data callback",
            || callbacks.load(Ordering::SeqCst) > after_recovery,
        );
        if id == 2 {
            assert_eq!(unsafe { count(0) }, 0, "successful resume must not prepare");
        } else {
            assert_eq!(unsafe { count(0) }, 1, "one prepare should recover the PCM");
            assert!(underruns.load(Ordering::SeqCst) >= 1);
        }
    } else if id == 7 {
        until("transient POLLERR was not injected", || unsafe {
            count(2) > 0
        });
        let after_error = callbacks.load(Ordering::SeqCst);
        until("transient POLLERR blocked a running PCM", || {
            callbacks.load(Ordering::SeqCst) > after_error
        });
        assert_eq!(
            unsafe { count(0) },
            0,
            "running PCM must not blindly prepare"
        );
    } else if matches!(id, 5 | 6) {
        until("disconnection not classified by actual worker", || {
            disconnected.load(Ordering::SeqCst) == 1
        });
        let events = unsafe { count(2) };
        std::thread::sleep(Duration::from_millis(30));
        assert_eq!(
            unsafe { count(2) },
            events,
            "disconnected worker kept polling"
        );
        assert_eq!(
            unsafe { count(0) },
            0,
            "disconnect must not blindly prepare"
        );
    } else if id == 4 {
        until(
            "suspended worker did not attempt resume",
            || unsafe { count(1) } >= 2,
        );
        assert_eq!(
            unsafe { count(0) },
            0,
            "pending resume must not blindly prepare"
        );
    }
    let stopped = Instant::now();
    drop(stream); // joins the actual worker; external parent also imposes a deadline.
    assert!(
        stopped.elapsed() < Duration::from_secs(1),
        "Drop did not terminate the worker"
    );
}
#[test]
fn pollerr_xrun_recovers_callback() {
    scenario(1, "pollerr_xrun_recovers_callback");
}
#[test]
fn pollerr_suspend_resumes_callback() {
    scenario(2, "pollerr_suspend_resumes_callback");
}
#[test]
fn pollerr_suspend_unsupported_prepares_callback() {
    scenario(3, "pollerr_suspend_unsupported_prepares_callback");
}
#[test]
fn drop_terminates_pending_suspend() {
    scenario(4, "drop_terminates_pending_suspend");
}
#[test]
fn pollhup_classifies_disconnect_and_stops() {
    scenario(5, "pollhup_classifies_disconnect_and_stops");
}
#[test]
fn pollerr_disconnected_state_stops() {
    scenario(6, "pollerr_disconnected_state_stops");
}

#[test]
fn pollerr_running_pcm_keeps_callback() {
    scenario(7, "pollerr_running_pcm_keeps_callback");
}
#[test]
fn pollerr_running_state_avail_epipe_recovers() {
    scenario(8, "pollerr_running_state_avail_epipe_recovers");
}

#[test]
fn healthy_null_pcm_still_plays_and_drops() {
    scenario(0, "healthy_null_pcm_still_plays_and_drops");
}
