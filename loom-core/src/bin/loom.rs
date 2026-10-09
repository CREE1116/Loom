fn main() {
    match loom_core::surfaces::cli() {
        Ok(code) => std::process::exit(code),
        Err(error) => {
            eprintln!("loom: {error:#}");
            std::process::exit(1);
        }
    }
}
