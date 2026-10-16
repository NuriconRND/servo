//! The response-signing public key is chosen at build time and nowhere else.

fn main() {
    println!("cargo::rerun-if-env-changed=SDWALL_DEVICE_AUTH_SIGNING_KEY");
    println!("cargo::rustc-check-cfg=cfg(sdwall_test_signing_key)");
    match std::env::var("SDWALL_DEVICE_AUTH_SIGNING_KEY").as_deref() {
        Err(_) | Ok("") | Ok("release") => {},
        Ok("test") => println!("cargo::rustc-cfg=sdwall_test_signing_key"),
        Ok(other) => {
            panic!("SDWALL_DEVICE_AUTH_SIGNING_KEY must be `release` or `test`, got `{other}`")
        },
    }
}
