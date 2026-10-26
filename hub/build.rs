//! kumacord-server.exe's icon and version info (`tools/build_winres.rs`).

#[path = "../tools/build_winres.rs"]
mod winres;

fn main() {
    winres::embed(std::path::Path::new("../app/assets/logo/akumalabs.ico"), "kumacord-server.exe", "kumacord-server");
}
