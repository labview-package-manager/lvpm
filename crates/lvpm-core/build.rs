//! Record which VIs this build goes with.
//!
//! `relink-package.vi` and `build-app.vi` ship next to the executable
//! (dist-workspace.toml, wix/main.wxs) instead of inside it: Microsoft Defender
//! flagged an unsigned exe that writes a blob out and hands it to another
//! process. Only their hashes are compiled in, so lvpm can refuse a VI left
//! over from another build.

use md5::{Digest, Md5};

fn main() {
    for (file, var) in [
        ("relink-package.vi", "LVPM_RELINK_VI_MD5"),
        ("build-app.vi", "LVPM_BUILD_APP_VI_MD5"),
    ] {
        let path = format!("src/lv-src/{file}");
        println!("cargo::rerun-if-changed={path}");
        let bytes = std::fs::read(&path).unwrap_or_else(|e| panic!("cannot read {path}: {e}"));
        let hex: String = Md5::digest(&bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        println!("cargo::rustc-env={var}={hex}");
    }
}
