//! Embeds the icon and version info into the Windows executable.

fn main() {
    println!("cargo:rerun-if-changed=assets/icon.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let mut res = winresource::WindowsResource::new();
    res.set_icon("assets/icon.ico")
        .set("ProductName", "Simple Converter")
        .set("FileDescription", "Simple Converter")
        .set("LegalCopyright", "MIT License");
    res.compile().expect("cannot embed Windows resources (is the Windows SDK installed?)");
}
