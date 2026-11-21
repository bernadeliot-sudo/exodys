use std::{env, path::PathBuf, process::Command};

fn main() {
    match env::var("CARGO_CFG_TARGET_OS").as_deref() {
        Ok("macos") => {
            println!("cargo:rerun-if-changed=native/system_proxy.m");
            cc::Build::new()
                .file("native/system_proxy.m")
                .compile("exodys_system_proxy");
            println!("cargo:rustc-link-lib=framework=SystemConfiguration");
            println!("cargo:rustc-link-lib=framework=CoreFoundation");
            println!("cargo:rustc-link-lib=framework=Security");
        }
        Ok("windows") if env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") => {
            embed_windows_icon();
        }
        _ => {}
    }
}

fn embed_windows_icon() {
    println!("cargo:rerun-if-changed=native/exodys.rc");
    println!("cargo:rerun-if-changed=assets/Exodys.ico");
    println!("cargo:rerun-if-env-changed=RC");

    let target = env::var("TARGET").expect("Cargo supplies TARGET");
    let mut compiler = match env::var_os("RC") {
        Some(path) => Command::new(path),
        None => {
            cc::windows_registry::find(&target, "rc.exe").unwrap_or_else(|| Command::new("rc.exe"))
        }
    };
    let output = PathBuf::from(env::var_os("OUT_DIR").expect("Cargo supplies OUT_DIR"))
        .join("exodys-icon.res");
    let status = compiler
        .args(["/I", "assets", "/FO"])
        .arg(&output)
        .arg("native/exodys.rc")
        .status()
        .expect("run the Windows SDK resource compiler (rc.exe), or set RC to llvm-rc");
    assert!(
        status.success(),
        "Windows icon resource compilation failed: {status}"
    );
    println!("cargo:rustc-link-arg-bin=exodys={}", output.display());
}
