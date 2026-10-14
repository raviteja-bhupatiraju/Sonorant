//! Developer self-tests, reachable only through hidden command-line flags:
//!
//! * `--selftest-paste`   inserts text into a test text box that Sonorant creates itself (and refuses
//!   to send any keystrokes unless that box has the focus);
//! * `--selftest-overlay` shows the recording indicator in each of its states;
//! * `--selftest-mic`     records one second and prints only level statistics;
//! * `--selftest-vram`    loads / switches / unloads the speech models and prints GPU memory use.

use std::time::Duration;

use anyhow::{bail, Result};

use crate::ui::overlay::Kind;
use crate::platform::Overlay;

#[cfg(windows)]
pub use win_paste::{paste, pump};

/// Wait `dur` (the Linux indicator runs on its own thread; nothing to dispatch here).
#[cfg(not(windows))]
fn pump(dur: Duration) {
    std::thread::sleep(dur);
}

/// `--selftest-paste` drives a Win32 edit box, so it exists on Windows only.
#[cfg(windows)]
mod win_paste {
use std::time::{Duration, Instant};

use anyhow::{bail, Result};
use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
use windows_sys::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DestroyWindow, DispatchMessageW, GetForegroundWindow, GetWindowTextW, PeekMessageW,
    SetForegroundWindow, SetWindowTextW, ShowWindow, TranslateMessage, ES_MULTILINE, MSG, PM_REMOVE, SW_SHOW,
    WS_OVERLAPPEDWINDOW, WS_VISIBLE,
};

use crate::config::PasteMethod;
use crate::platform::{self, open_clipboard, win::wide, InsertOptions};

/// Dispatch window messages for `dur` (the test windows live on this thread).
pub fn pump(dur: Duration) {
    let end = Instant::now() + dur;
    while Instant::now() < end {
        unsafe {
            let mut msg: MSG = std::mem::zeroed();
            while PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_REMOVE) != 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn window_text(hwnd: HWND) -> String {
    let mut buf = vec![0u16; 4096];
    let n = unsafe { GetWindowTextW(hwnd, buf.as_mut_ptr(), buf.len() as i32) };
    String::from_utf16_lossy(&buf[..n.max(0) as usize])
}

pub fn paste() -> Result<()> {
    let hwnd = unsafe {
        let h = CreateWindowExW(
            0,
            wide("EDIT").as_ptr(),
            wide("Sonorant paste test").as_ptr(),
            WS_OVERLAPPEDWINDOW | WS_VISIBLE | ES_MULTILINE as u32,
            200,
            200,
            600,
            260,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            GetModuleHandleW(std::ptr::null()),
            std::ptr::null(),
        );
        ShowWindow(h, SW_SHOW);
        // Windows only lets the foreground application hand out the focus: attach to its input queue.
        let fg_thread = GetWindowThreadProcessId(GetForegroundWindow(), std::ptr::null_mut());
        let me = GetCurrentThreadId();
        AttachThreadInput(me, fg_thread, 1);
        SetForegroundWindow(h);
        AttachThreadInput(me, fg_thread, 0);
        h
    };
    pump(Duration::from_millis(600));
    if unsafe { GetForegroundWindow() } != hwnd {
        unsafe { DestroyWindow(hwnd) };
        bail!("the test window could not take the focus; no keystrokes were sent");
    }

    let text = "Hello w\u{f6}rld, test 123.";
    let mut clipboard = open_clipboard().ok_or_else(|| anyhow::anyhow!("clipboard busy"))?;
    let mut failures = 0;
    for (name, method, keep) in [
        ("Ctrl+V", PasteMethod::CtrlV, true),
        ("Shift+Insert", PasteMethod::ShiftInsert, true),
        ("Ctrl+Shift+V", PasteMethod::CtrlShiftV, true),
        ("Typing", PasteMethod::Typing, true),
    ] {
        unsafe { SetWindowTextW(hwnd, wide("").as_ptr()) };
        clipboard.set_text("ORIGINAL-CLIPBOARD")?;
        if unsafe { GetForegroundWindow() } != hwnd {
            bail!("lost the focus before testing {name}; stopping");
        }
        let opts = InsertOptions { method, keep_clipboard: keep, auto_submit: false, trailing_space: true, cancelled: &|| false };
        let t = text.to_owned();
        let worker = std::thread::spawn(move || platform::insert(&t, &opts));
        while !worker.is_finished() {
            pump(Duration::from_millis(20));
        }
        pump(Duration::from_millis(200));
        let result = worker.join().map_err(|_| anyhow::anyhow!("paste thread panicked"))?;
        let got = window_text(hwnd);
        let expected = format!("{text} ");
        let clip_ok = clipboard.get_text().map(|c| c == "ORIGINAL-CLIPBOARD").unwrap_or(false);
        // Ctrl+Shift+V is not a paste shortcut in a plain Windows edit control, so it is only
        // expected to work in terminals and similar applications.
        let expected_to_work = method != PasteMethod::CtrlShiftV;
        let ok = result.is_ok() && ((got == expected) == expected_to_work || !expected_to_work);
        println!(
            "{name:<13} -> {:?}  text {}  clipboard restored: {}{}",
            got,
            if got == expected { "OK" } else { "different" },
            clip_ok,
            if expected_to_work { "" } else { "  (not a paste shortcut in a plain edit box)" }
        );
        if !ok || (expected_to_work && !clip_ok && method != PasteMethod::Typing) {
            failures += 1;
        }
    }
    unsafe { DestroyWindow(hwnd) };
    if failures > 0 {
        bail!("{failures} paste method(s) misbehaved");
    }
    println!("paste self-test passed");
    Ok(())
}
}

pub fn overlay() -> Result<()> {
    let overlay = Overlay::new().ok_or_else(|| anyhow::anyhow!("could not create the overlay window"))?;
    let long = "So this is a much longer dictation to check how the indicator copes with a lot of text. \
        It should wrap at a sensible width, stop after a few lines and end with an ellipsis instead of growing \
        past the screen, because nobody wants a wall of text covering the application they are typing into, \
        and the rest of this sentence will certainly be cut off before it is ever displayed on screen.";
    let cases: [(Kind, &str, Option<&str>); 8] = [
        (Kind::Recording, "Recording", None),
        (Kind::Recording, "Recording", Some("Loading the speech model...")),
        (Kind::Recording, long, None), // a long live transcript keeps its newest words
        (Kind::Working, "Transcribing...", Some("The first transcription after loading can be slow")),
        (Kind::Done, "Hello there, this is a short transcript.", None),
        (Kind::Done, "Medium length: please remember to buy milk, eggs & bread, and call Dr. Smith about the 3 p.m. appointment.", None),
        (Kind::Done, long, None),
        (Kind::Notice, "No speech detected", None),
    ];
    for (i, (kind, text, hint)) in cases.into_iter().enumerate() {
        overlay.show(kind, text, hint);
        println!("showing case {}: {}", i + 1, text.chars().take(40).collect::<String>());
        pump(Duration::from_secs(3));
    }
    overlay.hide();
    Ok(())
}

pub fn mic() -> Result<()> {
    let rec = crate::stt::recorder::Recorder::spawn(std::sync::Arc::new(|| println!("the microphone was lost")));
    let (tx, rx) = std::sync::mpsc::channel();
    let _live = rec.start("", Box::new(move |r| drop(tx.send(r))));
    rx.recv()?.map_err(|e| anyhow::anyhow!(e))?;
    std::thread::sleep(Duration::from_secs(1));
    let pcm = rec.stop_blocking();
    let rms = (pcm.iter().map(|x| x * x).sum::<f32>() / pcm.len().max(1) as f32).sqrt();
    let peak = pcm.iter().fold(0.0f32, |m, x| m.max(x.abs()));
    println!("captured {} samples at 16 kHz ({:.2} s), rms {:.4}, peak {:.3}", pcm.len(), pcm.len() as f32 / 16_000.0, rms, peak);
    anyhow::ensure!(pcm.len() > 12_000, "far too little audio captured");
    Ok(())
}

/// GPU memory in use according to nvidia-smi (MiB), if available.
fn vram_used_mib() -> Option<u32> {
    let out = std::process::Command::new("nvidia-smi")
        .args(["--query-gpu=memory.used", "--format=csv,noheader,nounits"])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout).lines().next()?.trim().parse().ok()
}

pub fn vram() -> Result<()> {
    use std::sync::Arc;

    use parking_lot::RwLock;

    use crate::config::{Config, SttModel};
    use crate::stt::{Stt, SttState};

    let cfg = Config { stt_enabled: true, use_vad: false, stt_model: SttModel::ParakeetUnified, ..Config::default() };
    let cfg = Arc::new(RwLock::new(cfg));
    let stt = Stt::spawn(cfg.clone(), Arc::new(|_| {}));

    let wait_ready = |stt: &Stt| -> Result<()> {
        std::thread::sleep(Duration::from_millis(300));
        for _ in 0..600 {
            match stt.status.lock().state.clone() {
                SttState::Ready => return Ok(()),
                SttState::Error(e) => bail!("{e}"),
                _ => std::thread::sleep(Duration::from_millis(100)),
            }
        }
        bail!("timed out waiting for the model")
    };
    let mib = || vram_used_mib().map(|m| m.to_string()).unwrap_or_else(|| "n/a".into());

    println!("baseline                      : {} MiB used", mib());
    stt.preload();
    wait_ready(&stt)?;
    println!("Parakeet loaded ({:<26}): {} MiB used", stt.status.lock().device, mib());

    cfg.write().stt_model = SttModel::Canary180M;
    stt.preload();
    wait_ready(&stt)?;
    println!("switched to Canary ({:<19}): {} MiB used", stt.status.lock().device, mib());

    cfg.write().stt_model = SttModel::ParakeetUnified;
    stt.preload();
    wait_ready(&stt)?;
    println!("switched back to Parakeet     : {} MiB used", mib());

    stt.unload();
    std::thread::sleep(Duration::from_secs(2));
    println!("unloaded (idle timeout path)  : {} MiB used", mib());
    stt.shutdown();
    Ok(())
}
