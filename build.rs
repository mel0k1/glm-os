fn main() {
    // cargo должен перелинковывать ядро при изменении linker script
    println!("cargo:rerun-if-changed=linker.ld");
}
