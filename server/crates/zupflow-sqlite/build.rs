// `sqlx::migrate!` embeds the files in `migrations/`, but Cargo only sees the
// ones that already exist: without this, a new migration isn't compiled in
// until something else changes, and the schema test checks a stale schema.
fn main() {
    println!("cargo:rerun-if-changed=migrations");
}
