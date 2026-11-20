fn main() {
    let mut cc = cc::Build::new();
    for lib in ["libavcodec", "libavutil", "libdrm"] {
        let l = pkg_config::probe_library(lib).unwrap_or_else(|e| panic!("{lib}: {e}"));
        for p in l.include_paths {
            cc.include(p);
        }
    }
    cc.file("src/enc.c").flag("-O2").compile("luchenc");
    println!("cargo:rerun-if-changed=src/enc.c");
}
