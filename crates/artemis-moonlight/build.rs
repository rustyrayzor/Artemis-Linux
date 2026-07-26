fn main() {
    #[cfg(target_os = "linux")]
    build_linux();
}

#[cfg(target_os = "linux")]
fn build_linux() {
    let destination = cmake::Config::new("../../native")
        .define("CMAKE_BUILD_TYPE", "Release")
        .define("BUILD_SHARED_LIBS", "OFF")
        .build();
    println!(
        "cargo:rustc-link-search=native={}/lib",
        destination.display()
    );
    println!("cargo:rustc-link-lib=static=artemis_moonlight_shim");
    println!("cargo:rustc-link-lib=static=moonlight-common-c");
    println!("cargo:rustc-link-lib=static=enet");
    println!("cargo:rustc-link-lib=dylib=crypto");
    println!("cargo:rustc-link-lib=dylib=pthread");
    println!("cargo:rustc-link-lib=dylib=m");
    println!("cargo:rerun-if-changed=../../native");
    println!("cargo:rerun-if-changed=../../vendor/moonlight-common-c/src");
}
