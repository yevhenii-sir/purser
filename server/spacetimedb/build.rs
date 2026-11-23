//! Compresses the admin page for the module to embed: `$OUT_DIR/admin.html.gz` and its ETag
//! (`ADMIN_ETAG`). The page itself (`admin/index.html`, built by `server/admin-ui`) stays in the
//! repository; the compressed copy is made here so it never has to be committed.

use sha2::Digest;
use std::io::Write;

fn main() {
    let src = "admin/index.html";
    println!("cargo:rerun-if-changed={src}");
    println!("cargo:rerun-if-changed=build.rs");
    let html = std::fs::read(src).expect("admin/index.html is missing: run `npm run build` in server/admin-ui");
    // No file name, no time: the header is the fixed 10 bytes `handlers::admin_page` relies on.
    let mut gz = flate2::GzBuilder::new().mtime(0).write(Vec::new(), flate2::Compression::best());
    gz.write_all(&html).expect("gzip");
    let bytes = gz.finish().expect("gzip");
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
    std::fs::write(out.join("admin.html.gz"), bytes).expect("write admin.html.gz");
    let etag: String = sha2::Sha256::digest(&html)[..8].iter().map(|b| format!("{b:02x}")).collect();
    println!("cargo:rustc-env=ADMIN_ETAG={etag}");
}
