fn main() {
    // rust-embed needs the folder to exist at compile time; an empty dist still compiles
    // (the server then answers "frontend not built").
    let dist = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../frontend/dist");
    if !dist.exists() {
        let _ = std::fs::create_dir_all(&dist);
    }
    println!("cargo:rerun-if-changed=../frontend/dist");
}
