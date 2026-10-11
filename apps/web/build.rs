//! Rebuild when a migration is added: `sqlx::migrate!` embeds the folder at
//! compile time but does not watch it for new files on stable Rust.
fn main() {
    println!("cargo:rerun-if-changed=../../migrations/sqlite");
}
