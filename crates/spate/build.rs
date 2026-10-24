include!("winres.rs");

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=winres.rs");
    println!("cargo:rerun-if-changed=src/icon_art.rs");
    embed_windows_resources("Spate download manager", "spate.exe");
}
