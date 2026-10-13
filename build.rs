fn main() {
    // sqlx::migrate! embeds existing files; directory tracking also rebuilds
    // the binary when a new forward migration is added during development.
    println!("cargo:rerun-if-changed=migrations");
}
