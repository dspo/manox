fn main() {
    if let Err(err) = cx::run() {
        eprintln!("错误: {err:#}");
        std::process::exit(1);
    }
}
