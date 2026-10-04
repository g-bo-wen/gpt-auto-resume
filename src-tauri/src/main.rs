#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
fn main() {
    if std::env::args().any(|a| a == "--observe-only") {
        auto_resume::observe_only();
    } else {
        auto_resume::run();
    }
}
