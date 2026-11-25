use std::{env, path::PathBuf, process::Command};

fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows")
        || env::var_os("CARGO_FEATURE_DESKTOP").is_none()
    {
        return;
    }

    println!("cargo::rerun-if-changed=packaging/windows/AppIcon.ico");
    println!("cargo::rerun-if-changed=packaging/windows/app.rc");
    let output_dir = PathBuf::from(env::var_os("OUT_DIR").expect("Cargo output directory"));
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
