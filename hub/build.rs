//! concord-server.exe's icon and version info (`tools/build_winres.rs`): the same logo as the app.

#[path = "../tools/build_winres.rs"]
mod winres;

fn main() {
    winres::embed(std::path::Path::new("../app/assets/logo/concord.ico"), "concord-server.exe", "concord-server");
}
