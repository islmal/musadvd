fn main() -> std::process::ExitCode {
    musadvd::cli::run(std::env::args_os())
}
