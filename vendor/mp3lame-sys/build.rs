//! Builds LAME 3.100's encoder with the `cc` crate on every target.
//!
//! astar patch (see VENDORED.md): upstream runs autotools `configure` and
//! `make` on unix, which needs autoconf, make and a shell on the build
//! machine and does not cross-compile to musl cleanly. This compiles the
//! same `libmp3lame/*.c` files directly, with the checked-in
//! `astar/config.h` standing in for what `configure` would write. The
//! decoder (`mpglib`) stays out unless the `decoder` feature asks for it,
//! as upstream's `--disable-decoder` default does.

const LAME_DIR: &str = "lame-3.100";

const SOURCES: &[&str] = &[
    "bitstream.c",
    "encoder.c",
    "fft.c",
    "gain_analysis.c",
    "id3tag.c",
    "lame.c",
    "newmdct.c",
    "presets.c",
    "psymodel.c",
    "quantize_pvt.c",
    "quantize.c",
    "reservoir.c",
    "set_get.c",
    "tables.c",
    "takehiro.c",
    "util.c",
    "vbrquantize.c",
    "VbrTag.c",
    "version.c",
];

fn main() {
    if std::env::var("DOCS_RS").is_ok_and(|d| d == "1") {
        return;
    }
    let dir = std::path::Path::new(LAME_DIR);
    let arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    let features = std::env::var("CARGO_CFG_TARGET_FEATURE").unwrap_or_default();
    let mut cc = cc::Build::new();
    for s in SOURCES {
        cc.file(dir.join("libmp3lame").join(s));
    }
    cc.include("astar")
        .include(dir.join("include"))
        .include(dir.join("libmp3lame"))
        .define("HAVE_CONFIG_H", None)
        .warnings(false)
        .extra_warnings(false);
    let x86 = arch == "x86" || arch == "x86_64";
    if x86 {
        cc.define("TAKEHIRO_IEEE754_HACK", None);
    }
    if x86 && features.split(',').any(|f| f == "sse") {
        cc.define("HAVE_XMMINTRIN_H", None)
            .define("MIN_ARCH_SSE", None)
            .file(dir.join("libmp3lame/vector/xmm_quantize_sub.c"))
            .include(dir.join("libmp3lame/vector"));
    }
    if std::env::var("CARGO_FEATURE_DECODER").is_ok() {
        cc.define("HAVE_MPGLIB", None)
            .define("DECODE_ON_THE_FLY", None)
            .include(dir.join("mpglib"))
            .file(dir.join("libmp3lame/mpglib_interface.c"));
        for s in [
            "common.c",
            "dct64_i386.c",
            "decode_i386.c",
            "interface.c",
            "layer1.c",
            "layer2.c",
            "layer3.c",
            "tabinit.c",
        ] {
            cc.file(dir.join("mpglib").join(s));
        }
    }
    cc.compile("mp3lame");
    println!("cargo:rerun-if-changed=astar/config.h");
    println!("cargo:rerun-if-changed=build.rs");
}
