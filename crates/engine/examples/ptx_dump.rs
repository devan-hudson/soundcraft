//! Prints what the `.ptx` reader finds in each file named on the command line.
fn main() {
    for a in std::env::args().skip(1) {
        let b = std::fs::read(&a).unwrap_or_default();
        println!("{a}: {:#?}", soundcraft_engine::io::parse_ptx(&b));
    }
}
