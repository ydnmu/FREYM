fn main() {
    println!("cargo:rustc-check-cfg=cfg(frame_linux_pulse)");
    println!("cargo:rerun-if-changed=native/application_audio.c");
    println!("cargo:rustc-check-cfg=cfg(frame_linux_va_import)");
    println!("cargo:rerun-if-changed=native/va_import.c");
    println!("cargo:rerun-if-env-changed=DOCS_RS");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("linux") {
        return;
    }
    // Cross-host Rust checking can cover the FFI seam without pretending to
    // compile or link a native Linux library.
    if std::env::var_os("DOCS_RS").is_some() {
        println!("cargo:rustc-cfg=frame_linux_va_import");
        println!("cargo:rustc-cfg=frame_linux_pulse");
        return;
    }
    // Optional protocol adapter: absence keeps PipeWire microphones available.
    if let Ok(pulse) = pkg_config::Config::new()
        .atleast_version("13")
        .probe("libpulse")
    {
        let mut build = cc::Build::new();
        build.file("native/application_audio.c").flag("-std=c11");
        for include in pulse.include_paths {
            build.include(include);
        }
        build.compile("freym_application_audio");
        println!("cargo:rustc-cfg=frame_linux_pulse");
    }
    // Older installations still build and use the existing SHM pipeline.
    let va = pkg_config::Config::new()
        .atleast_version("1.24")
        .cargo_metadata(false)
        .probe("gstreamer-va-1.0");
    let libva = pkg_config::Config::new()
        .atleast_version("1.15")
        .cargo_metadata(false)
        .probe("libva");
    let (Ok(va), Ok(libva)) = (va, libva) else {
        println!(
            "cargo:warning=DMA-BUF import needs GStreamer VA 1.24 and libva 1.15 development libraries; using SHM"
        );
        return;
    };
    let allocators = pkg_config::Config::new()
        .cargo_metadata(false)
        .probe("gstreamer-allocators-1.0")
        .expect("GStreamer VA requires the allocator development library");
    let mut build = cc::Build::new();
    build.file("native/va_import.c").flag("-std=c11");
    for library in [&va, &libva, &allocators] {
        for include in &library.include_paths {
            build.include(include);
        }
    }
    build.compile("freym_va_import");
    for name in ["gstreamer-va-1.0", "gstreamer-allocators-1.0", "libva"] {
        pkg_config::Config::new().probe(name).unwrap();
    }
    println!("cargo:rustc-cfg=frame_linux_va_import");
}
