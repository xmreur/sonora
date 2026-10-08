fn main() {
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os == "android" {
        return;
    }
    println!("cargo:rerun-if-changed=shim/widevine_shim.cc");
    println!("cargo:rerun-if-changed=vendor/content_decryption_module.h");
    cc::Build::new()
        .cpp(true)
        .file("shim/widevine_shim.cc")
        .include("vendor")
        .flag_if_supported("-std=c++14")
        .flag_if_supported("-Wno-unused-parameter")
        .compile("widevine_shim");
    match target_os.as_str() {
        "macos" | "ios" => println!("cargo:rustc-link-lib=dylib=c++"),
        "windows" => {}
        _ => {
            println!("cargo:rustc-link-lib=dylib=stdc++");
            println!("cargo:rustc-link-lib=dylib=dl");
        }
    }
}
