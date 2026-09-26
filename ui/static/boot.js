// Loader for the Rust/WebAssembly dashboard (wasm-bindgen glue). The whole
// application is Rust; browsers simply require a module script to start WASM.
import init from "./wm_ui.js";
init({ module_or_path: "./wm_ui_bg.wasm" });
