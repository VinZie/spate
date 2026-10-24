// Shares the icon and version-info embedding with the desktop app.
include!("../spate/winres.rs");

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=../spate/winres.rs");
    println!("cargo:rerun-if-changed=../spate/src/icon_art.rs");
    embed_windows_resources("Spate command line and browser helper", "spate-cli.exe");
}
