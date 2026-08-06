fn main() {
    match harness_manox::replay::run_bundled() {
        Ok(report) => {
            println!("{}", serde_json::to_string_pretty(&report).unwrap());
            if !report.acceptance_passed {
                std::process::exit(1);
            }
        }
        Err(error) => {
            eprintln!("context replay failed: {error}");
            std::process::exit(2);
        }
    }
}
