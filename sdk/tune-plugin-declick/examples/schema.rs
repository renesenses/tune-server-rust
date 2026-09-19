fn main() {
    println!(
        "{}",
        serde_json::to_string_pretty(&schemars::schema_for!(tune_plugin_declick::Options)).unwrap()
    );
}
