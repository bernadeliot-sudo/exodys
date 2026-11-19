pub mod catalog;
pub mod engine;
pub mod import;
pub mod session;

#[cfg(target_os = "macos")]
mod ui;
#[cfg(target_os = "macos")]
pub use ui::App;

#[cfg(target_os = "macos")]
#[nsh_core::prelude::native_module("app/relay-desk")]
fn native(registry: &mut nsh_core::prelude::Registry) {
    registry.register::<App>();
}

pub type Result<T> = std::result::Result<T, String>;
