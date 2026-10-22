fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }
    println!("cargo:rerun-if-changed=native/frame_macos.m");
    cc::Build::new()
        .file("native/frame_macos.m")
        .flag("-fobjc-arc")
        .flag("-fblocks")
        .flag("-mmacosx-version-min=14.0")
        .compile("frame_macos_native");
    for framework in [
        "Foundation",
        "AppKit",
        "CoreGraphics",
        "CoreMedia",
        "CoreVideo",
        "AVFoundation",
        "ScreenCaptureKit",
        "VideoToolbox",
        "CoreAudio",
    ] {
        println!("cargo:rustc-link-lib=framework={framework}");
    }
}
