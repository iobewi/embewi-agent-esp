fn main() {
    let out_dir = std::env::var("OUT_DIR").unwrap();
    std::fs::copy("workload.ld", format!("{out_dir}/workload.ld")).unwrap();
    println!("cargo:rustc-link-search={out_dir}");
    println!("cargo:rerun-if-changed=workload.ld");
}
