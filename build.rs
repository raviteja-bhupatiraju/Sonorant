use std::path::{Path, PathBuf};

#[path = "src/ui/icon.rs"]
mod icon;

/// Encode the app icon as a multi-size .ico (uncompressed 32-bit DIB frames).
fn make_ico() -> Vec<u8> {
    let sizes = [16u32, 24, 32, 48, 64, 256];
    let frames: Vec<Vec<u8>> = sizes
        .iter()
        .map(|&s| {
            let rgba = icon::icon_rgba(s, true);
            let mut f = Vec::new();
            f.extend_from_slice(&40u32.to_le_bytes());
            f.extend_from_slice(&(s as i32).to_le_bytes());
            f.extend_from_slice(&((s * 2) as i32).to_le_bytes()); // XOR + AND mask height
            f.extend_from_slice(&1u16.to_le_bytes());
            f.extend_from_slice(&32u16.to_le_bytes());
            f.extend_from_slice(&[0u8; 24]);
            for y in (0..s).rev() {
                for x in 0..s {
                    let i = ((y * s + x) * 4) as usize;
                    f.extend_from_slice(&[rgba[i + 2], rgba[i + 1], rgba[i], rgba[i + 3]]);
                }
            }
            let mask_row = (s.div_ceil(32) * 4) as usize;
            f.resize(f.len() + mask_row * s as usize, 0);
            f
        })
        .collect();

    let mut out = Vec::new();
    out.extend_from_slice(&[0, 0, 1, 0]);
    out.extend_from_slice(&(sizes.len() as u16).to_le_bytes());
    let mut offset = 6 + 16 * sizes.len();
    for (s, f) in sizes.iter().zip(&frames) {
        let dim = if *s >= 256 { 0 } else { *s as u8 };
        out.extend_from_slice(&[dim, dim, 0, 0]);
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&32u16.to_le_bytes());
        out.extend_from_slice(&(f.len() as u32).to_le_bytes());
        out.extend_from_slice(&(offset as u32).to_le_bytes());
        offset += f.len();
    }
    for f in frames {
        out.extend_from_slice(&f);
    }
    out
}

/// Encode the app icon as a PNG for Linux menus and packages. The image data is stored in
/// uncompressed deflate blocks, so no compression library is needed (256 px is ~260 KB).
fn make_png(size: u32) -> Vec<u8> {
    fn crc32(bytes: &[u8]) -> u32 {
        !bytes.iter().fold(!0u32, |crc, &b| {
            (0..8).fold(crc ^ b as u32, |c, _| if c & 1 != 0 { (c >> 1) ^ 0xedb8_8320 } else { c >> 1 })
        })
    }
    fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        let start = out.len();
        out.extend_from_slice(kind);
        out.extend_from_slice(data);
        let crc = crc32(&out[start..]);
        out.extend_from_slice(&crc.to_be_bytes());
    }
    // Scanlines, each prefixed with filter type 0 (none).
    let raw: Vec<u8> = icon::icon_rgba(size, true).chunks(size as usize * 4).flat_map(|row| [&[0u8][..], row].concat()).collect();
    let mut zlib = vec![0x78, 0x01];
    let blocks: Vec<&[u8]> = raw.chunks(0xffff).collect();
    for (i, block) in blocks.iter().enumerate() {
        let len = block.len() as u16;
        zlib.push((i + 1 == blocks.len()) as u8); // BFINAL on the last block, BTYPE 00 (stored)
        zlib.extend_from_slice(&len.to_le_bytes());
        zlib.extend_from_slice(&(!len).to_le_bytes());
        zlib.extend_from_slice(block);
    }
    let (a, b) = raw.iter().fold((1u32, 0u32), |(a, b), &x| ((a + x as u32) % 65521, (b + a + x as u32) % 65521));
    zlib.extend_from_slice(&((b << 16) | a).to_be_bytes());

    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut header = Vec::new();
    header.extend_from_slice(&size.to_be_bytes());
    header.extend_from_slice(&size.to_be_bytes());
    header.extend_from_slice(&[8, 6, 0, 0, 0]); // 8-bit RGBA
    chunk(&mut png, b"IHDR", &header);
    chunk(&mut png, b"IDAT", &zlib);
    chunk(&mut png, b"IEND", &[]);
    png
}

/// Copy the shared libraries (`*.so*`) in `src` to `dest`, keeping version symlinks as symlinks.
#[cfg(unix)]
fn stage_shared_libs(src: &Path, dest: &Path) {
    let Ok(entries) = std::fs::read_dir(src) else { return };
    for entry in entries.flatten() {
        let (path, name) = (entry.path(), entry.file_name());
        if !name.to_string_lossy().contains(".so") {
            continue;
        }
        let target = dest.join(&name);
        let _ = std::fs::remove_file(&target);
        let staged = match std::fs::read_link(&path) {
            Ok(link) => std::os::unix::fs::symlink(link, &target),
            Err(_) => std::fs::copy(&path, &target).map(drop),
        };
        if let Err(e) = staged {
            println!("cargo:warning=could not stage {}: {e}", path.display());
        }
    }
}

#[cfg(not(unix))]
fn stage_shared_libs(_src: &Path, _dest: &Path) {}

fn main() {
    // Build scripts run on the host: the target comes from Cargo's environment, not `cfg!`.
    let windows = std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows");
    // ONNX Runtime (Kokoro) and the Whistle engine are loaded at run time. Put them (if vendored)
    // next to the built executable so `cargo run` works.
    let libs = if windows { ["onnxruntime.dll", "libneedle3.dll"] } else { ["libonnxruntime.so", "libneedle3.so"] };
    println!("cargo:rerun-if-changed=src/ui/icon.rs");
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
    // OUT_DIR = target/<profile>/build/<pkg>-<hash>/out
    for lib in libs {
        println!("cargo:rerun-if-changed=vendor/{lib}");
        let src = PathBuf::from("vendor").join(lib);
        if let (true, Some(dir)) = (src.is_file(), out_dir.ancestors().nth(3)) {
            let _ = std::fs::copy(&src, dir.join(lib));
        }
    }
    if !windows {
        // transcribe.cpp stages its libraries next to the executable on Windows only. Do the same
        // here (it loads its backend modules from its own folder) and look for them there.
        if let Some(dir) = out_dir.ancestors().nth(3) {
            // The icon for menus and packages (`installer/build-linux.sh`), next to the binary.
            let _ = std::fs::write(dir.join("sonorant.png"), make_png(256));
            for var in ["DEP_TRANSCRIBE_CPP_RUNTIME_DIR", "DEP_TRANSCRIBE_CPP_MODULE_DIR"] {
                println!("cargo:rerun-if-env-changed={var}");
                if let Some(src) = std::env::var_os(var) {
                    stage_shared_libs(Path::new(&src), dir);
                }
            }
        }
        println!("cargo:rustc-link-arg-bins=-Wl,-rpath,$ORIGIN");
        return;
    }

    // espeak-ng (statically linked via espeak-rs-sys) reads the registry on Windows.
    println!("cargo:rustc-link-lib=advapi32");

    // Embed the speaker icon as the exe's icon (Explorer, taskbar, Alt+Tab).
    let ico = out_dir.join("sonorant.ico");
    std::fs::write(&ico, make_ico()).expect("write ico");
    let mut res = winresource::WindowsResource::new();
    res.set_icon(ico.to_str().unwrap());
    res.set("ProductName", "Sonorant");
    res.set("CompanyName", "Ravi Teja Bhupatiraju");
    res.set("FileDescription", "Sonorant text-to-speech");
    res.set("LegalCopyright", "Copyright (c) 2026 Ravi Teja Bhupatiraju. MIT License.");
    if let Err(e) = res.compile() {
        println!("cargo:warning=could not embed icon: {e}");
    }
}
