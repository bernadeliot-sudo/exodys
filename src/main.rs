#[cfg(target_os = "macos")]
fn main() -> std::process::ExitCode {
    use nsh_platform_macos::{AppOptions, AppSource, Root, run_app};

    run_app(
        AppSource::Rust(Root::of::<relay_desk::App>()),
        AppOptions {
            app_id: "io.github.bernadeliot-sudo.relay-desk".into(),
            title: "Relay Desk".to_owned(),
            size: (960, 700),
            ..AppOptions::default()
        },
    )
}

#[cfg(not(target_os = "macos"))]
fn main() -> std::process::ExitCode {
    eprintln!("Relay Desk's desktop interface requires macOS.");
    std::process::ExitCode::FAILURE
}
