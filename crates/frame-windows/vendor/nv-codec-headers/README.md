# NVENC header provenance

Unmodified NVIDIA `nvEncodeAPI.h`, distributed through the FFmpeg project's nv-codec-headers SDK-header mirror. This is not a dependency on the FFmpeg encoder or CLI.

- Source: [nv-codec-headers commit e844e5b](https://github.com/FFmpeg/nv-codec-headers/blob/e844e5b26f46bb77479f063029595293aa8f812d/include/ffnvcodec/nvEncodeAPI.h)
- Tag: `n13.0.19.0`; commit: `e844e5b26f46bb77479f063029595293aa8f812d`
- SDK API: 13.0
- SHA-256: `4fe4094541ef0f8a13249d97a8692dc5f835a6e9dd42eeadb3e2f7321d54dc7e`
- NVIDIA copyright and permissive license are retained in the file verbatim.

Do not hand-edit the header or transcribe its structures, bitfields, function table or version macros into Rust. Updates must pin a new source revision, preserve its notice, record its hash, compile the ABI checks and rerun native initialization/encode/drain tests. Runtime use must negotiate the installed driver's supported API version and load its library from the Windows system directory. Header availability proves no hardware capability.

ADR 0006 selects a small C ABI bridge compiled against this header, with opaque session handles and checked POD arguments. Only Frame's bridge ABI is declared in Rust; SDK layouts stay in C. This avoids build-time CUDA/NVDEC/link-library requirements of the evaluated `nvidia-video-codec-sdk` 0.4.0 package. Dependency count and Cargo.lock stability were not the deciding criteria.
