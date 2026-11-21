//! Embeds the application icon in the executable.

fn main() {
    println!("cargo:rerun-if-changed=naelbar.rc");
    println!("cargo:rerun-if-changed=assets/naelbar.ico");
    if std::env::var_os("CARGO_CFG_WINDOWS").is_some() {
        embed_resource::compile("naelbar.rc", embed_resource::NONE).manifest_optional().unwrap();
    }
}
