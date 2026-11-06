// `sqlx::migrate!` embeds migrations/ at compile time, and cargo does not
// otherwise notice a file added there: without this, a change that only adds
// a migration builds a binary that never applies it.
fn main() {
    println!("cargo:rerun-if-changed=migrations");
}
