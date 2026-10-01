//! Leptos CSR frontend for PDF Tools.
#![cfg_attr(
    not(test),
    deny(clippy::expect_used, clippy::panic, clippy::unwrap_used)
)]

#[cfg(target_arch = "wasm32")]
mod browser;
#[cfg(any(target_arch = "wasm32", test))]
mod files;
#[cfg(any(target_arch = "wasm32", test))]
mod imposition;
#[cfg(any(target_arch = "wasm32", test))]
mod presentation;
#[cfg(any(target_arch = "wasm32", test))]
mod transport;
#[cfg(any(target_arch = "wasm32", test))]
mod workspace;

#[cfg(target_arch = "wasm32")]
mod app;

/// Mounts the client-side application.
#[cfg(target_arch = "wasm32")]
#[wasm_bindgen::prelude::wasm_bindgen(start)]
pub fn main() {
    leptos::mount::mount_to_body(app::App);
}
