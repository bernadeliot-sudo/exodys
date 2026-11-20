pub mod catalog;
pub mod engine;
pub mod import;
mod routing;
pub mod session;
mod settings_file;
#[cfg(target_os = "macos")]
pub mod system_proxy;
#[cfg(not(target_os = "macos"))]
mod system_proxy {
    use crate::Result;
    pub(crate) struct Lease;
    impl Lease {
        pub fn acquire(_: u16) -> Result<Self> {
            Err("系统代理仅支持 macOS。".into())
        }
        pub fn release(self) -> Result<()> {
            Ok(())
        }
        pub fn alive(&mut self) -> bool {
            false
        }
    }
    pub fn recover() -> Result<()> {
        Ok(())
    }
}

#[cfg(target_os = "macos")]
mod macos_native;
#[cfg(target_os = "macos")]
mod ui;
#[cfg(target_os = "macos")]
pub use ui::App;

#[cfg(target_os = "macos")]
#[nsh_core::prelude::native_module("app/exodys")]
fn native(registry: &mut nsh_core::prelude::Registry) {
    registry.register::<App>();
}

#[cfg(target_os = "macos")]
pub fn register_native_components(components: &nsh_platform_macos::Components) {
    macos_native::register_components(components);
}

#[cfg(target_os = "macos")]
#[nsh_core::element::native_components(macos)]
fn native_components(components: &nsh_platform_macos::Components) {
    register_native_components(components);
}

pub type Result<T> = std::result::Result<T, String>;
