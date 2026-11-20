fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        // The native bridge statically links Swift code, which uses the OS Swift runtime.
        println!("cargo:rustc-link-arg=-Wl,-rpath,/usr/lib/swift");
        let path = std::path::PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap())
            .join("Info.plist");
        println!("cargo:rerun-if-changed=Info.plist");
        println!(
            "cargo:rustc-link-arg=-Wl,-sectcreate,__TEXT,__info_plist,{}",
            path.display()
        );
    }
}
