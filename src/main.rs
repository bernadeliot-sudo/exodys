#[cfg(target_os = "macos")]
fn main() -> std::process::ExitCode {
    use nsh_platform_macos::{AppOptions, AppSource, Root, run_app};

    let options = AppOptions {
        app_id: "io.github.bernadeliot-sudo.exodys".into(),
        title: "Exodys".to_owned(),
        size: (960, 700),
        ..AppOptions::default()
    };
    exodys::register_native_components(&options.components);
    run_app(AppSource::Rust(Root::of::<exodys::App>()), options)
}

#[cfg(not(target_os = "macos"))]
fn main() -> std::process::ExitCode {
    eprintln!("Exodys's desktop interface requires macOS.");
    std::process::ExitCode::FAILURE
}
