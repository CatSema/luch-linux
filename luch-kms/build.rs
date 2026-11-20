fn main() {
    let mut cc = cc::Build::new();
    for lib in ["libavcodec", "libavutil", "libdrm", "egl", "glesv2", "libva"] {
        for p in pkg_config::probe_library(lib).unwrap_or_else(|e| panic!("{lib}: {e}")).include_paths {
            cc.include(p);
        }
    }
    cc.file("src/kms.c").flag("-O2").compile("luchkms");
    println!("cargo:rerun-if-changed=src/kms.c");
}
