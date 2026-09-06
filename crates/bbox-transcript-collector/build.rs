fn main() {
    println!("cargo:rerun-if-changed=macos/Info.plist");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }

    // This standalone launch agent is its own responsible code. Embed the
    // identity and purpose macOS needs for Local Network privacy attribution;
    // metadata never grants access or substitutes for the operator's signature.
    let manifest = std::path::PathBuf::from(
        std::env::var_os("CARGO_MANIFEST_DIR").expect("Cargo supplies the manifest directory"),
    );
    let plist = manifest.join("macos/Info.plist");
    // Separate linker arguments preserve paths containing spaces or commas.
    for argument in ["-sectcreate", "__TEXT", "__info_plist"] {
        println!("cargo:rustc-link-arg-bin=bbox-transcript-collector=-Xlinker");
        println!("cargo:rustc-link-arg-bin=bbox-transcript-collector={argument}");
    }
    println!("cargo:rustc-link-arg-bin=bbox-transcript-collector=-Xlinker");
    println!(
        "cargo:rustc-link-arg-bin=bbox-transcript-collector={}",
        plist.display()
    );
}
