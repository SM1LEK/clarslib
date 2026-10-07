mod cli;

fn main() {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if let Err(error) = cli::run(&args) {
        eprintln!("Ошибка: {error}");
        std::process::exit(1);
    }
}
