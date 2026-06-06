// build.rs — No-op build script for the playful-cloud-core plugin crate.
//
// tauri-build is only required in the *application* crate (playful-cloud-desktop),
// not in a plugin library crate. Plugin permission manifests are resolved by the
// consuming app's build script, not by the plugin itself.
fn main() {}
