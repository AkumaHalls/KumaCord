//! AkumaLabs.exe's icon and version info (`tools/build_winres.rs`).

#[path = "../tools/build_winres.rs"]
mod winres;

fn main() {
    winres::embed(std::path::Path::new("assets/logo/akumalabs.ico"), "AkumaLabs.exe", "AkumaLabs");
}
