fn main() {
    let schemas = serde_json::json!({
      "manifest":schemars::schema_for!(tune_plugin_sdk::manifest::Manifest),
      "commands":schemars::schema_for!(tune_plugin_sdk::ui::UiRequest),
      "events":schemars::schema_for!(tune_plugin_sdk::ui::AudioLevelsEvent),
      "jobs":schemars::schema_for!(tune_plugin_sdk::batch::JobResult)
    });
    println!("{}", serde_json::to_string_pretty(&schemas).unwrap());
}
