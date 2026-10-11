//! Rebuild when a migration is added: `sqlx::migrate!` embeds the folders at
//! compile time but does not watch them for new files on stable Rust.
fn main() {
    println!("cargo:rerun-if-changed=../../migrations/sqlite");
    println!("cargo:rerun-if-changed=../../migrations/postgres");
}
