use std::{env, path::PathBuf, process::Command};

#[cfg(feature = "server")]
fn compress_web_assets(output: &std::path::Path) -> std::io::Result<()> {
    use base64::{Engine, engine::general_purpose::STANDARD};
    use sha2::{Digest, Sha256};
    use flate2::{Compression, write::GzEncoder};
    use std::{fs, io::Write, path::Path};
    println!("cargo::rerun-if-changed=src/online/login-gate.js");
    // Keep ordinary Rust builds offline while detecting stale generated assets.
    let read_script = |name: &str| -> std::io::Result<String> {
        let path = Path::new("src/online").join(name);
        println!("cargo::rerun-if-changed={}", path.display());
        Ok(fs::read_to_string(path)?.replace("\r\n", "\n").replace('\r', "\n"))
    };
    let sources = [read_script("calendar.js")?, read_script("calendar-wire.mjs")?].join("\0");
    let script = read_script("calendar.min.js")?;
    for (text, expected) in [
        (&sources, include_str!("src/online/calendar.source.sha256")),
        (&script, include_str!("src/online/calendar.min.sha256")),
    ] {
        assert_eq!(format!("{:x}", Sha256::digest(text.as_bytes())), expected.trim(),
            "Regenerate calendar.min.js with scripts/minify-calendar.py");
    }
    let digest = Sha256::digest(script.as_bytes());
    let path = format!("/static/calendar-{digest:x}.js");
    let integrity = format!("sha256-{}", STANDARD.encode(digest));
    println!("cargo::rustc-env=GODOVOY_SCRIPT_PATH={path}");
    println!("cargo::rustc-env=GODOVOY_SCRIPT_INTEGRITY={integrity}");
    for name in [
        "warm.html",
        "index.html",
        "calendar.js",
        "calendar.css",
        "admin.html",
        "admin.js",
        "admin.css",
    ] {
        let source = Path::new("src/online").join(if name == "warm.html" { "index.html" } else { name });
        println!("cargo::rerun-if-changed={}", source.display());
        let mut input = fs::read(source)?;
        if name == "calendar.js" {
            input = script.as_bytes().to_vec();
            fs::write(output.join(name), &input)?;
        }
        if matches!(name, "index.html" | "warm.html") {
            let mut document = String::from_utf8(input).expect("UTF-8 calendar template");
            for (marker, asset, end_tag) in [
                ("<!-- CALENDAR_STYLE -->", "calendar.css", "</style"),
                ("/* LOGIN_GATE */", "login-gate.js", "</script"),
                ("/* CALENDAR_SCRIPT */", "calendar.min.js", "</script"),
            ] {
                if name == "warm.html" && asset == "calendar.min.js" {
                    let original = format!("<script>{marker}</script>");
                    assert_eq!(document.matches(&original).count(), 1);
                    let external = format!(r#"<script id="calendar-program" src="{path}" integrity="{integrity}"></script>"#);
                    document = document.replacen(&original, &external, 1);
                    continue;
                }
                let text = fs::read_to_string(Path::new("src/online").join(asset))?;
                assert_eq!(document.matches(marker).count(), 1, "missing or repeated {marker}");
                assert!(!text.to_ascii_lowercase().contains(end_tag), "unexpected {end_tag} in {asset}");
                document = document.replacen(marker, &text, 1);
            }
            input = document.into_bytes();
            fs::write(output.join(name), &input)?;
        }
        let file = fs::File::create(output.join(format!("{name}.gz")))?;
        let mut encoder = GzEncoder::new(file, Compression::best());
        encoder.write_all(&input)?;
        encoder.finish()?;
        let mut compressed = fs::File::create(output.join(format!("{name}.br")))?;
        let params = brotli::enc::BrotliEncoderParams {
            quality: 11,
            ..Default::default()
        };
        brotli::BrotliCompress(&mut input.as_slice(), &mut compressed, &params)?;
    }
    Ok(())
}

fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    let output_dir = PathBuf::from(env::var_os("OUT_DIR").expect("Cargo output directory"));
    #[cfg(feature = "server")]
    compress_web_assets(&output_dir).expect("compress static web assets");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows")
        || env::var_os("CARGO_FEATURE_DESKTOP").is_none()
    {
        return;
    }

    println!("cargo::rerun-if-changed=packaging/windows/AppIcon.ico");
    println!("cargo::rerun-if-changed=packaging/windows/app.rc");
    let target = env::var("TARGET").expect("Cargo build target");
    let (mut compiler, object) = if target.ends_with("-msvc") {
        println!("cargo::rerun-if-env-changed=RC");
        let mut rc = Command::new(env::var_os("RC").unwrap_or_else(|| "rc.exe".into()));
        let output = output_dir.join("app-icon.res");
        rc.args(["/nologo", "/fo"]).arg(&output).arg("app.rc");
        (rc, output)
    } else {
        println!("cargo::rerun-if-env-changed=WINDRES");
        let arch = target.split('-').next().expect("Rust target architecture");
        let tool =
            env::var_os("WINDRES").unwrap_or_else(|| format!("{arch}-w64-mingw32-windres").into());
        let mut windres = Command::new(tool);
        let output = output_dir.join("app-icon.o");
        windres
            .args(["-i", "app.rc", "-O", "coff", "-o"])
            .arg(&output);
        (windres, output)
    };
    let status = compiler
        .current_dir("packaging/windows")
        .status()
        .unwrap_or_else(|error| {
            panic!("Couldn't run Windows icon resource compiler {compiler:?}: {error}")
        });
    assert!(
        status.success(),
        "Windows icon resource compiler failed: {status}"
    );
    println!(
        "cargo::rustc-link-arg-bin=godovoy-plan={}",
        object.display()
    );
}
