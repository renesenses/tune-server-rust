fn main() {
    println!(
        "{}",
        serde_json::to_string_pretty(&schemars::schema_for!(
            tune_plugin_channel_remap::ChannelRemapSettings
        ))
        .unwrap()
    );
}
