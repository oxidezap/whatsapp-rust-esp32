fn main() {
    embuild::espidf::sysenv::output();

    // Expose .env values as compile-time env vars. A missing .env is fine: main.rs
    // reads these via option_env! with defaults, so a fresh clone still builds.
    if let Ok(iter) = dotenvy::dotenv_iter() {
        for item in iter.flatten() {
            println!("cargo:rustc-env={}={}", item.0, item.1);
        }
    }
    println!("cargo:rerun-if-changed=.env");
}
