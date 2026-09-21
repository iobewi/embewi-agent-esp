fn main() {
    // Our own linker script instead of esp-hal's `linkall.x`: the ROM only
    // loads RAM segments of a second-stage bootloader, so *everything* --
    // code, rodata, data -- must live in RAM (see boot.x).
    let dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    println!("cargo:rustc-link-search={dir}");
    println!("cargo:rustc-link-arg=-Tboot.x");
    println!("cargo:rerun-if-changed=boot.x");
}
