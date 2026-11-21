#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

#[cfg(target_os = "macos")]
fn main() -> std::process::ExitCode {
    use nsh_platform_macos::{AppOptions, AppSource, Root, run_app};

    if let Some(result) = exodys::system_proxy::helper_entry() {
        return result;
    }
    let mut args = std::env::args_os().skip(1);
    if let Some(flag) = args.next()
        && (flag == "--check-tun-config" || flag == "--prepare-tun-config")
    {
        let result = (|| {
            let dns = args
                .next()
                .and_then(|arg| arg.to_str()?.parse::<std::net::IpAddr>().ok())
                .filter(|_| args.next().is_none())
                .ok_or_else(|| {
                    format!("用法：exodys {} <DNS 服务器 IP>", flag.to_string_lossy())
                })?;
            let directory = exodys::catalog::default_directory()?;
            let session = exodys::session::Session::open(directory)?;
            if flag == "--prepare-tun-config" {
                let (candidate, id) = session.prepare_tun_config(dns)?;
                println!("配置编号 {id}\n私有配置文件夹 {}", candidate.display());
            } else {
                session.check_tun_config(dns)?;
                println!("sing-box 已通过 TUN 配置校验。未启动隧道。");
            }
            Ok::<(), String>(())
        })();
        return match result {
            Ok(()) => std::process::ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("{error}");
                std::process::ExitCode::FAILURE
            }
        };
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
