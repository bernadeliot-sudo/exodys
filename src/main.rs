#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

#[cfg(target_os = "macos")]
fn main() -> std::process::ExitCode {
    use nsh_platform_macos::{AppOptions, AppSource, Root, run_app};

    if let Some(result) = exodys::system_proxy::helper_entry() {
        return result;
    }
    let options = AppOptions {
        app_id: "io.github.bernadeliot-sudo.exodys".into(),
        title: "Exodys".to_owned(),
        size: (960, 700),
        ..AppOptions::default()
    };
    exodys::register_native_components(&options.components);
    run_app(AppSource::Rust(Root::of::<exodys::App>()), options)
}

#[cfg(target_os = "windows")]
fn main() -> std::process::ExitCode {
    use nsh_platform_windows::{AppOptions, AppSource, Root, run_app};

    run_app(
        AppSource::Rust(Root::of::<exodys::App>()),
        AppOptions {
            app_id: "io.github.bernadeliot-sudo.exodys".into(),
            title: "Exodys".to_owned(),
            size: (960, 700),
            ..AppOptions::default()
        },
    )
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn main() -> std::process::ExitCode {
    eprintln!("Exodys's desktop interface requires macOS or Windows.");
    std::process::ExitCode::FAILURE
}
