fn main() {
    println!("cargo:rerun-if-changed=res/geoboost.rc");
    println!("cargo:rerun-if-changed=res/geoboost.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        embed_resource::compile("res/geoboost.rc", embed_resource::NONE).manifest_optional().unwrap();
    }
}
