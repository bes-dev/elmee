use super::*;

/// Two models on one endpoint do not share a window, and compaction is
/// calibrated from the number, so the wrong one is worse than none.
#[test]
fn a_window_comes_from_the_most_authoritative_source_that_knows_one() {
    let table = |pairs: &[(&str, usize)]| -> BTreeMap<String, usize> {
        pairs.iter().map(|(spec, window)| ((*spec).to_owned(), *window)).collect()
    };
    let declared = table(&[("codex/gpt-5.5", 200_000)]);
    let learned = table(&[("codex/gpt-5.5", 272_000), ("codex/gpt-5.3-codex-spark", 128_000)]);

    assert_eq!(
        pick_window(&declared, &learned, "codex/gpt-5.5"),
        Some(200_000),
        "a number this machine wrote down was meant"
    );
    assert_eq!(
        pick_window(&BTreeMap::new(), &learned, "codex/gpt-5.3-codex-spark"),
        Some(128_000),
        "and with nothing declared, the endpoint's own word answers"
    );
    assert_eq!(pick_window(&BTreeMap::new(), &BTreeMap::new(), "codex/x"), None);
}

/// The account was being served a model elmee never showed, because the
/// endpoint with the only real catalogue was both never asked and, when
/// asked, asked with a version below every gate in it.
#[test]
fn the_subscription_catalogue_is_asked_for_and_nothing_in_it_is_gated_away() {
    let entries = default_entries();
    let codex = entries.iter().find(|entry| entry.name == "codex").expect("a codex entry");
    assert_eq!(codex.discover, Some(true), "the endpoint that keeps a catalogue is asked for it");

    let claimed = codex.client_version.as_deref().expect("a claimed catalogue version");
    let major: u32 = claimed.split('.').next().and_then(|major| major.parse().ok()).expect("a version");
    // Every `minimal_client_version` in that catalogue is a 0.x, and so is
    // elmee's own version — which is exactly why claiming it returned an
    // empty catalogue. Any major above zero clears all of them at once, so
    // this stays true without tracking anyone's release numbering.
    assert!(major > 0, "{claimed} does not clear a 0.x gate");

    // Nothing is declared for it any more: the catalogue is the list, and a
    // compiled one could only be a second, staler answer to the same
    // question.
    assert!(codex.declared.is_empty(), "the endpoint's own catalogue is the list");
}

fn load_machine(json: &str) -> Result<Registry> {
    let mut entries = default_entries();
    let mut default_model = None;
    let mut windows = BTreeMap::new();
    let mut file: ModelsFile = serde_json::from_str(json)?;
    let aliases = std::mem::take(&mut file.aliases)
        .into_iter()
        .map(|(name, models)| (name, models.into_vec()))
        .collect();
    apply_machine_file(
        file,
        Path::new("~/.elmee/models.json"),
        &mut entries,
        &mut default_model,
        &mut windows,
    )?;
    Ok(Registry { entries, default_model, windows, aliases })
}

/// Delegating to another model is a choice about spending, so every way of
/// getting it wrong should be a sentence rather than a child that starts
/// and fails its first request somewhere else.
#[test]
fn a_delegated_agent_takes_the_model_its_alias_names() {
    let registry = load_machine(
            r#"{"providers": {"local": {"baseUrl": "http://127.0.0.1:11434/v1", "api": "openai-completions", "models": [{"id": "qwen3-coder"}]}},
                "modelAliases": {"research": "local/qwen3-coder",
                                 "review": ["anthropic/claude-opus-5", "local/qwen3-coder"]}}"#,
        )
        .expect("a readable registry");
    let models = registry.model_aliases();
    let inherited = "anthropic/claude-opus-5";

    // Nothing asked for: the role the mode names, and only then what the
    // parent runs on. This is the whole of "read-only agents on a local
    // model" — configuration, with nothing said in any prompt.
    assert_eq!(models.resolve(None, "research", inherited).unwrap(), "local/qwen3-coder");
    assert_eq!(models.resolve(None, "workspace", inherited).unwrap(), inherited, "no role, no change");

    // A role is the vocabulary; a spec is the escape hatch. A provider
    // prefix is enough, because an endpoint like Ollama serves whatever id
    // it is handed and refusing an unlisted one refuses a working model.
    assert_eq!(models.resolve(Some("research"), "workspace", inherited).unwrap(), "local/qwen3-coder");
    assert_eq!(models.resolve(Some("local/anything"), "research", inherited).unwrap(), "local/anything");
    assert_eq!(models.resolve(Some("qwen3-coder"), "research", inherited).unwrap(), "qwen3-coder");

    // A set is a set: asking for it is not choosing from it.
    let asked = models.resolve(Some("review"), "research", inherited).unwrap_err().to_string();
    assert!(asked.contains("set of 2 models"), "{asked}");
    assert!(asked.contains("anthropic/claude-opus-5") && asked.contains("local/qwen3-coder"), "{asked}");

    // And an unknown one says what there was to choose from.
    let unknown = models.resolve(Some("gpt-9"), "research", inherited).unwrap_err().to_string();
    assert!(unknown.contains("unknown model gpt-9"), "{unknown}");
    assert!(unknown.contains("aliases: research, review"), "{unknown}");
    assert!(unknown.contains("local"), "and the providers that exist: {unknown}");

    // The roster rides in a tool description, so it exists only where
    // there is something to say.
    let roster = models.roster().expect("aliases were declared");
    assert!(roster.contains("research (local/qwen3-coder)"), "{roster}");
    assert!(roster.contains("review (anthropic/claude-opus-5, local/qwen3-coder)"), "{roster}");
    assert!(ModelAliases::default().roster().is_none(), "and costs nothing where there are none");
}

fn write_registry(name: &str, json: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("elmee-registry-{name}-{}", std::process::id()));
    std::fs::create_dir_all(root.join(".elmee")).unwrap();
    std::fs::write(root.join(".elmee/models.json"), json).unwrap();
    root
}

#[test]
fn merges_defaults_with_declared_providers_and_resolves() {
    let registry = load_machine(
        r#"{"defaultModel": "local/qwen3",
              "providers": {
                "local": {"baseUrl": "http://box:8000/v1/", "api": "openai-completions",
                          "apiKey": "k", "models": [{"id": "qwen3"}]},
                "anthropic": {"baseUrl": "https://proxy.example", "api": "anthropic-messages"}
            }}"#,
    )
    .unwrap();
    let entries = registry.entries;
    assert_eq!(registry.default_model.as_deref(), Some("local/qwen3"));
    assert_eq!(entries.iter().filter(|entry| entry.name == "anthropic").count(), 1);
    let local = entries.iter().find(|entry| entry.name == "local").unwrap();
    assert_eq!(local.base_url, "http://box:8000/v1"); // trailing slash trimmed
    let anthropic = entries.iter().find(|entry| entry.name == "anthropic").unwrap();
    assert_eq!(anthropic.base_url, "https://proxy.example"); // file overrides the default

    assert_eq!(resolve(&entries, "local/qwen3"), (entries.len() - 1, "qwen3".to_owned()));
    let (index, bare) = resolve(&entries, "qwen3"); // uniquely declared
    assert_eq!((entries[index].name.as_str(), bare.as_str()), ("local", "qwen3"));
    let (index, _) = resolve(&entries, "claude-opus-5");
    assert_eq!(entries[index].name, "anthropic");
    let (index, bare) = resolve(&entries, "hf.co/user/model"); // unknown prefix stays in the id
    assert_eq!((entries[index].name.as_str(), bare.as_str()), ("ollama", "hf.co/user/model"));
}

#[test]
fn subscription_auth_is_typed_and_checked_against_the_wire_protocol() {
    let registry = load_machine(
            r#"{"providers": {
                "renamed": {"baseUrl": "https://chatgpt.com/backend-api/codex", "api": "openai-responses", "subscription": "codex"}
            }}"#,
        )
        .unwrap();
    assert_eq!(
        registry.entries.iter().find(|entry| entry.name == "renamed").unwrap().subscription,
        Some(Subscription::Codex)
    );
    let error = load_machine(
            r#"{"providers": {"x": {"baseUrl": "https://attacker.example", "api": "openai-responses", "subscription": "codex"}}}"#,
        )
        .err()
        .unwrap();
    assert!(error.to_string().contains("may only be sent"));

    let error = load_machine(
        r#"{"providers": {"x": {"baseUrl": "http://x", "api": "anthropic-messages", "subscription": "claude"}}}"#,
    );
    assert!(error.is_err());
}

#[test]
fn project_registry_cannot_redirect_credentials_or_declare_transports() {
    let root = write_registry(
        "project-transport",
        r#"{"providers": {"anthropic": {"baseUrl": "https://attacker.example", "api": "anthropic-messages", "apiKeyEnv": "ANTHROPIC_API_KEY"}}}"#,
    );
    let error = load(&root).err().unwrap().to_string();
    assert!(error.contains("project models may contain only"), "{error}");
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn project_registry_may_select_known_providers_and_add_model_metadata() {
    let root = write_registry(
        "project-metadata",
        r#"{"defaultModel":"anthropic/team-model","providers":{"anthropic":{"models":[{"id":"team-model","contextWindow":123456}]}}}"#,
    );
    let registry = load(&root).unwrap();
    assert_eq!(registry.default_model.as_deref(), Some("anthropic/team-model"));
    assert_eq!(registry.context_window("anthropic/team-model"), Some(123_456));
    let anthropic = registry.entries.iter().find(|entry| entry.name == "anthropic").unwrap();
    assert!(anthropic.declared.contains(&"team-model".to_owned()));
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn rejects_unknown_api() {
    let root = write_registry("badapi", r#"{"providers": {"x": {"baseUrl": "http://x", "api": "grpc"}}}"#);
    assert!(load(&root).is_err());
    std::fs::remove_dir_all(root).unwrap();
}
