fn main() {
    // A CLI whose output goes into a closed pipe (`memetag search | head`) should end quietly, as every other
    // command-line tool does. Rust starts with SIGPIPE ignored, which turns that into a panic on the next write;
    // the default disposition restores the convention. Found on the phone, 2026-10-04.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    memetag::cli_main();
}
