fn main() {
  if let Err(code) = gnostr_modal::run() {
    std::process::exit(code);
  }
}
