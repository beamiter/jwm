use jwm_eva::config::Config;

#[test]
fn shipped_configuration_is_accepted_by_the_actual_strict_loader() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("dist/eva.toml");
    let config = Config::load(Some(&path))
        .expect("the installed example must match the daemon's actual strict schema");
    assert!(!config.future_wake_word.enabled);
    assert!(!config.privacy.save_raw_audio);
    assert!(!config.privacy.include_clipboard_in_context);
    assert_eq!(config.openai.api_key_source, "secret_service");
    assert!(config.execution.require_guarded_execution);
}
