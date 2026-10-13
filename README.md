# Sonorant

System-tray speech for Windows and Linux (Wayland and X11). **Text to speech** with [Kokoro-82M](https://huggingface.co/hexgrad/Kokoro-82M): select text in any app,
press the hotkey (default **Ctrl+`**), and it is read aloud. **Speech recognition** with
[Cactus Whistle](https://huggingface.co/Cactus-Compute/whistle) (17 MB, seven languages, the default), NVIDIA Parakeet or Canary (on by default, choose the model or switch it off in Settings):
press a hotkey (default **F1**), speak, press it again, and the text is typed into the active application.
Everything runs on this computer; the models are downloaded once on first use. Settings live in a Dear ImGui dialog.

## Build and run

```
cargo run --release
```

### Windows

Needs a Rust toolchain, MSVC and CMake (espeak-ng is built from source).

`vendor/onnxruntime.dll` (ONNX Runtime 1.30, loaded dynamically, not committed) is fetched with `scripts/fetch-onnxruntime.ps1` and copied next to the exe by `build.rs`.
`vendor/libneedle3.dll`, the Whistle engine (loaded dynamically too), comes from `scripts/fetch-needle.ps1`.
The prebuilt static ORT that `ort` downloads by default failed to link with the older MSVC STL on this machine, hence `load-dynamic`.
Parakeet and Canary run on [transcribe.cpp](https://github.com/cjpais/transcribe.cpp), on the GPU through Vulkan when there is one (CPU otherwise).
Building that needs Vulkan headers and `glslc`: `scripts/setup-gpu-build.ps1` puts them in `vendor/vulkan-sdk` (git-ignored; `.cargo/config.toml` points at it).
Build without them using `cargo build --no-default-features` (CPU only).
`installer/build.ps1` builds the release and the Inno Setup installer.

### Linux (Wayland and X11)

Needs a stable Rust toolchain, a C/C++ compiler, CMake, `unzip` and, for the GPU speech backend, Vulkan headers and `glslc`:

* Arch: `pacman -S cmake clang alsa-lib unzip vulkan-headers shaderc vulkan-icd-loader onnxruntime-cpu`
* Debian/Ubuntu: `apt install cmake clang libclang-dev libasound2-dev pkg-config unzip libvulkan-dev glslc spirv-headers`

The Whistle engine is `vendor/libneedle3.so`, from `scripts/fetch-needle.sh`; `build.rs` copies it next to the binary.
ONNX Runtime is found in `/usr/lib` when the distribution packages it (Arch's `onnxruntime-cpu`); otherwise
`scripts/fetch-onnxruntime.sh` puts the official build in `vendor/libonnxruntime.so` and `build.rs` copies it next to the binary.
In place, espeak-ng finds its data in the build tree.

**Packages:** `installer/build-linux.sh` builds an AppImage, a `.deb` and an `.rpm` in `installer/output/`. It builds in a
Debian 12 container (podman or docker), so the packages run on glibc 2.36 and newer (Debian 12+, Ubuntu 23.04+, Fedora 37+);
`--native` builds on the host instead, for systems at least as new as it. Tools and build caches live in `~/.cache/sonorant-build`.
The packages install to `/usr/lib/sonorant` (binary, libraries, ONNX Runtime, the speech backends, English-only `espeak-ng-data`) with
`/usr/bin/sonorant`, a menu entry and an icon; the AppImage carries the same files.

## Models

Models are downloaded on first use into the standard **Hugging Face cache** (`HF_HUB_CACHE`, else `%HF_HOME%\hub`, else
`~/.cache/huggingface/hub`, shared with other tools); the settings dialog shows progress. Files already in the cache are reused.

| Purpose | Repository | File(s) |
|---|---|---|
| Kokoro TTS | `onnx-community/Kokoro-82M-v1.0-ONNX` | `onnx/model.onnx` (326 MB; the int8 `model_quantized.onnx` is downloaded only if chosen) + the 20 US voices in `voices/` |
| Cactus Whistle (default) | `Cactus-Compute/whistle` | `whistle.cact` (17 MB) |
| NVIDIA Parakeet Unified EN 0.6B | `handy-computer/parakeet-unified-en-0.6b-gguf` | `parakeet-unified-en-0.6b-Q8_0.gguf` (731 MB) |
| NVIDIA Canary 180M Flash | `handy-computer/canary-180m-flash-gguf` | `canary-180m-flash-Q8_0.gguf` (218 MB) |
| Silence removal | `onnx-community/silero-vad` | `onnx/model.onnx` (2 MB) |

Only the model you pick is downloaded. Whistle runs on Cactus's Needle engine (`libneedle3.dll` / `libneedle3.so`, a 1.5 MB library with no dependencies beyond the C runtime).

## Performance

* **Parakeet and Canary** use the GPU through Vulkan when one is available (Settings shows whether the model loaded on the GPU or the CPU, and
  `--cpu` forces the CPU in the command-line tools). They are faster than Whistle on a strong GPU (about 0.1 s for 7 s of speech on an RTX 4090 against
  about 0.5 s) and Parakeet is the most accurate on rare words and names. A model is unloaded after an idle timeout (Settings > Insertion > Advanced).
* **Whistle** runs on the CPU and needs no GPU. On a Core i9-13900K it transcribes up to about 5 s of speech in 50-150 ms; the
  engine slows down for longer clips (about 0.3 s more, then roughly 50 ms per extra second: 0.5 s for 7 s of speech, 1.7 s for 22 s),
  so a dictation of 8-15 s takes 0.5-1.1 s. The model loads in a few milliseconds on first use and stays loaded.
* **Kokoro** runs on the CPU. ONNX Runtime's DirectML provider (the only vendor-neutral GPU option for ONNX on Windows) fails on this
  model (`ConvTranspose` node error, tested), and the CPU already starts speaking after about 200 ms.
* Silero VAD (2 MB) runs on the CPU: it is faster than a GPU round trip for a model that small.

## Using it

* **Right-click the tray icon** for the menu: Settings…, Speak clipboard, Stop speaking, Quit. Left-click opens Settings too.
  On Linux the tray uses StatusNotifierItem (KDE Plasma and most panels; GNOME needs the AppIndicator extension).
* **Hotkey**: copies the selection (Ctrl+C, or Ctrl+Insert in consoles), reads it, restores your clipboard. Pressing it with nothing selected while speaking stops speech. A second hotkey (default Ctrl+1) stops speech.
* **Dictation** (Settings > Dictation): toggle mode (default: press to start, press to stop) or push-to-talk (hold), Esc cancels, a short tap is ignored in push-to-talk. A small indicator
  shows the recognised text live while you speak (streamed with Parakeet; Whistle and Canary re-transcribe about twice a second and once a second),
  then Transcribing. The inserted text comes from a final pass over the whole recording. Silence is removed with Silero VAD before transcribing.
* **Inserting text** (Settings > Insertion): Ctrl+V (Shift+Insert automatically in terminals), Ctrl+Shift+V, direct Unicode typing
  (any keyboard layout, no clipboard), or clipboard only; optional Enter after inserting, trailing space, clipboard restore.
  Custom words (vocabulary hints) and a **History** tab with Copy buttons.
* Config: `%APPDATA%\Sonorant\config.json` (Linux: `~/.config/Sonorant/config.json`). Log: `%LOCALAPPDATA%\Sonorant\sonorant.log`
  (Linux: `~/.local/share/Sonorant/sonorant.log`).
* Test without the GUI: `sonorant.exe --say "text" [--voice af_heart] [--speed 1.0] [--wav out.wav] [--mute] [--threads N] [--int8]` prints time-to-first-audio and real-time factor. `--settings` opens the dialog at startup (`--tab Dictation` selects a tab).
* Speech recognition without the GUI: `sonorant.exe --transcribe file.wav [--stt whistle|canary|parakeet] [--lang en] [--no-vad] [--live] [--cpu]`
  (`--live` first plays the file into the live transcript in real time and prints each partial result).
* Developer hooks: `--selftest-paste`, `--selftest-overlay`, `--selftest-mic`, `--selftest-vram` (Windows, NVIDIA: GPU memory while the speech models load and unload), and the `SONORANT_TEST_WAV=file.wav` environment variable,
  which makes the recorder return that audio instead of using the microphone (end-to-end tests without speaking).

## Linux desktops: Wayland and X11

Sonorant picks the backend from the session: Wayland when `WAYLAND_DISPLAY` is set, else X11 when `DISPLAY` is set
(`SONORANT_BACKEND=x11` or `wayland` overrides it). The same binary and packages serve both.

### X11 (Linux Mint MATE/Cinnamon, Xfce, i3...)

* **Hotkeys** are key grabs on the root window, so they work without any portal: press and release both arrive (push-to-talk works),
  Caps Lock and Num Lock do not matter, and a key another program already holds is reported in Settings. Plain Esc is claimed only while
  Sonorant is speaking or dictating, as on Windows.
* **Speak selection** reads the primary selection, like on Wayland.
* **Inserting text** sends the keys with the XTest extension: Ctrl+V (Ctrl+Shift+V automatically in terminals, which X11 lets Sonorant
  detect), Shift+Insert, or typing, including characters the keyboard layout lacks. Nothing needs to be installed or approved.
* **The indicator** is a small window at the bottom of the monitor under the pointer; translucent with a compositing manager, shaped
  without one. The tray icon needs a StatusNotifierItem host (Mint ships one; Xfce and others may need an applet).

### Wayland

Wayland does not let applications grab keys, send keystrokes or place windows, so Sonorant uses the desktop's own services:

* **Hotkeys** go through the GlobalShortcuts portal (KDE Plasma, GNOME 48+, Hyprland): Sonorant suggests its shortcuts and the
  desktop asks you to confirm them (press and release both arrive, so push-to-talk works). Elsewhere, bind commands in the
  desktop's keyboard settings: `sonorant --cmd speak`, `stop`, `dictate` (or `dictate-press` / `dictate-release` for push-to-talk),
  `cancel`, `clipboard`, `settings`, `quit`. They are passed to the running instance over a socket in `$XDG_RUNTIME_DIR`.
  Plain Esc cannot be a global shortcut there; the stop key also cancels a dictation.
* **Speak selection** reads the primary selection (whatever is selected), so no copy shortcut is simulated and the clipboard is
  untouched. The selection stays readable after it is deselected, so pressing the hotkey again on the same text while it is being
  read stops speech.
* **Inserting text** uses the RemoteDesktop portal (the desktop asks once; the permission is remembered), or `wtype` on wlroots
  compositors (Sway, Hyprland, river). The focused application is unknown, so Automatic paste is Ctrl+V; choose Ctrl+Shift+V for
  terminals.
* **The indicator** is a layer-shell overlay (KDE Plasma, wlroots compositors, Hyprland); GNOME has no layer shell, so there it is
  a desktop notification updated in place.
* Sonorant installs `~/.local/share/applications/org.sonorant.Sonorant.desktop` so portals can identify it; autostart writes
  `~/.config/autostart/sonorant.desktop`.

## Streaming design

Kokoro's ONNX graph is a single non-autoregressive pass (text tokens in, full waveform out), so audio cannot be streamed from *inside* one inference. Options considered:

1. Whole-text synthesis, then play: latency grows with text length. Rejected.
2. Fixed sentence-by-sentence: better, but the first sentence may be long.
3. **Chunked pipeline with a ramp (implemented)**: chunks are limited to 60, 130, 240, 320 characters. The first is a few words (~12 tokens, ~190 ms on an i9-13900K), and synthesis of chunk N+1 overlaps playback of chunk N. Each chunk is silence-trimmed and followed by a punctuation-based pause. Synthesis pauses when more than 6 s of audio is queued, and any new request or Stop cancels immediately (epoch counter).
4. Other ways to cut latency: persistent audio stream (no device start-up), model warm-up at load, 8 intra-op threads, punctuation kept in the phoneme string so the model produces natural pauses.

Measured here (fp32, 8 threads): first audio about 190–230 ms, RTF about 0.15. The int8 model was 3–4x *slower* on this CPU, so fp32 is the default.

## Limitations

* Windows and Linux (Wayland and X11). On Wayland, what works depends on the desktop's portals (see above); Linux was
  developed and tested under WSLg, so the portal paths are untested on real desktops, and the X11 paths were tested on a virtual X server (Xvfb), not yet on a real X11 desktop.
* US English only: only Kokoro's `af_*` / `am_*` voices are offered, and only US English pronunciation data is shipped.
* Windows: elevated (admin) windows cannot receive simulated keystrokes (copy, paste, typing) from a non-elevated Sonorant.
* Wayland: typing (rather than pasting) text the keyboard layout cannot produce depends on the desktop's RemoteDesktop portal.
* Whistle understands English, German, French, Spanish, Italian, Dutch and Polish; it is a very small model, so it can get rare words
  and names wrong (Custom words help). Parakeet is English only; Canary adds German, Spanish and French.
* Whistle's engine keeps one model per process, so unloading it after the idle timeout does not free its 17 MB.
* The Windows installer ships about 55 MB of engine modules (the Vulkan module alone is 44 MB uncompressed).
* The live transcript is a preview only: the text that is inserted is transcribed again from the whole recording after you stop.
* The first run needs a network connection to download the models (about 340 MB with the fp32 voice model). Sonorant needs
  `onnxruntime.dll` / `libonnxruntime.so`, the Whistle engine, `transcribe.dll` with the `ggml*.dll` modules (Linux: the matching `.so` files) and `espeak-ng-data` next to the program; the installers and packages place them there.

## License

Sonorant is released under the [MIT License](LICENSE). The components it uses keep their own licenses: Kokoro-82M (Apache 2.0),
ONNX Runtime, transcribe.cpp, ggml, Silero VAD and Dear ImGui (MIT), the Cactus Whistle model and Needle engine (Apache 2.0), the NVIDIA Parakeet and Canary models (CC-BY-4.0), and
eSpeak NG (GPL v3), whose library and data are included in the builds and installers.
