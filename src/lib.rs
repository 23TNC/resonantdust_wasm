//! Resonant Dust client-side wasm crate.
//!
//! Compiled to a browser wasm bundle (see the `wasm` service in
//! `compose.yml`) and imported by the pixijs client. The server does NOT
//! consume this crate — it links the shared logic crates directly as
//! rlibs. This crate is the eventual home for the DSL runtime + shared
//! logic that ships to the client as wasm.
//!
//! It re-exports the browser-facing surface of `resonantdust-data` (the
//! shared data-manipulation crate) through wasm-bindgen. The data crate
//! itself carries no JS bindings, so the server can link it as a plain
//! rlib; the bindings live only here, where they're needed.
//!
//! Right now it is a hello-world placeholder plus one vertical slice over
//! the data crate, so the build pipeline (`check` / `build` / `test` /
//! `wasm`) and the bindings -> data wiring can be verified before any
//! real code lands.

/// Plain Rust greeting — available in every build (native + wasm) so
/// `cargo check/build/test` work without the `js` feature.
pub fn greeting() -> String {
    "Hello, world from resonantdust-wasm!".to_string()
}

#[cfg(feature = "js")]
use wasm_bindgen::prelude::*;

/// Browser-facing greeting, exported through wasm-bindgen under the `js`
/// feature. Mirrors how `resonantdust-content` gates its JS API.
#[cfg(feature = "js")]
#[wasm_bindgen]
pub fn greet() -> String {
    greeting()
}

/// Vertical slice proving the bindings -> data-crate path: extract a
/// bit-field via `resonantdust_data::bits`. Real exports will follow this
/// shape — thin wasm-bindgen wrappers delegating to the data crate.
#[cfg(feature = "js")]
#[wasm_bindgen(js_name = getField)]
pub fn get_field(word: u32, offset: u32, width: u32) -> u32 {
    resonantdust_data::bits::get_field(word, offset, width)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn greeting_is_hello() {
        assert_eq!(greeting(), "Hello, world from resonantdust-wasm!");
    }
}
