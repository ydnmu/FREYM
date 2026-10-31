//! Native build steps for the Windows backend.
//!
//! Two C translation units are compiled with the workspace's accepted `cc`
//! dependency, both against the pinned NVIDIA header in `vendor/nv-codec-headers`:
//!
//! - `native/nvenc_header_check.c` is the integration-owned ABI prerequisite check.
//! - `native/nvenc_bridge.c` is the Frame-owned NVENC bridge (FRAME-0104): the only
//!   place where NVIDIA SDK structures, bitfields, GUIDs, version macros and the
//!   function table exist. Rust declares only the Frame-owned ABI from
//!   `native/nvenc_bridge.h`, and never a handwritten SDK layout or function table.
//!
//! Neither unit is compiled for a non-Windows target, so the Linux-target lint keeps
//! seeing a crate with no native encoder code rather than a stub.

fn main() {
    println!("cargo:rerun-if-changed=native/nvenc_header_check.c");
    println!("cargo:rerun-if-changed=native/nvenc_bridge.c");
    println!("cargo:rerun-if-changed=native/nvenc_bridge.h");
    println!("cargo:rerun-if-changed=vendor/nv-codec-headers/nvEncodeAPI.h");
    #[cfg(windows)]
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        cc::Build::new()
            .file("native/nvenc_header_check.c")
            .include("vendor/nv-codec-headers")
            .std("c11")
            .warnings_into_errors(true)
            .compile("frame_nvenc_header_check");

        // The NVENC bridge is an ABI boundary, not a library: it is compiled into the
        // crate and every entry point is declared in Rust from the Frame-owned header.
        cc::Build::new()
            .file("native/nvenc_bridge.c")
            .include("vendor/nv-codec-headers")
            .include("native")
            .std("c11")
            .warnings_into_errors(true)
            .compile("frame_nvenc_bridge");
    }
}
