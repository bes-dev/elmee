use super::*;

trait Greeting {
    fn text(&self) -> &'static str;
}
struct Hello;
impl Greeting for Hello {
    fn text(&self) -> &'static str {
        "hello"
    }
}

struct EmptyPlugin;
impl Plugin for EmptyPlugin {
    fn id(&self) -> &'static str {
        "empty"
    }
    fn declare(self, _plugins: &mut Registrar) -> Result<()> {
        Ok(())
    }
}

#[test]
fn plugin_ids_are_unique() {
    let mut plugins = Registry::new();
    plugins.install(EmptyPlugin).unwrap();
    assert!(plugins.install(EmptyPlugin).is_err());
    assert_eq!(plugins.installed(), &["empty"]);
}

#[test]
fn services_are_open_typed_and_may_have_multiple_providers() {
    let services = Services::default();
    services.provide::<dyn Greeting>("one", Rc::new(Hello));
    assert_eq!(services.one::<dyn Greeting>().unwrap().unwrap().text(), "hello");
    services.provide::<dyn Greeting>("two", Rc::new(Hello));
    assert_eq!(services.all::<dyn Greeting>().len(), 2);
    assert!(services.one::<dyn Greeting>().is_err());
}

#[test]
fn continuation_tools_cannot_shadow_installed_tools() {
    struct OneTool(Vec<serde_json::Value>);
    impl Handler for OneTool {
        fn definitions(&self) -> &[serde_json::Value] {
            &self.0
        }
        fn execute<'a>(&'a mut self, _: &'a ToolCall) -> PluginFuture<'a, ToolOutcome> {
            Box::pin(async { ToolOutcome::new(String::new(), true) })
        }
    }
    struct Shadow(Vec<serde_json::Value>);
    impl Handler for Shadow {
        fn definitions(&self) -> &[serde_json::Value] {
            &self.0
        }
        fn execute<'a>(&'a mut self, _: &'a ToolCall) -> PluginFuture<'a, ToolOutcome> {
            Box::pin(async { ToolOutcome::new(String::new(), true) })
        }
    }
    struct ShadowPlugin(Shadow);
    impl Plugin for ShadowPlugin {
        fn id(&self) -> &'static str {
            "shadow"
        }
        fn declare(self, registrar: &mut Registrar) -> Result<()> {
            registrar.provide_handler(self.0)
        }
    }
    let schema = serde_json::json!({"type":"function","function":{"name":"same"}});
    let mut plugins = Registry::new();
    struct One(OneTool);
    impl Plugin for One {
        fn id(&self) -> &'static str {
            "one"
        }
        fn declare(self, registrar: &mut Registrar) -> Result<()> {
            registrar.provide_handler(self.0)
        }
    }
    plugins.install(One(OneTool(vec![schema]))).unwrap();
    assert!(
        plugins
            .install(ShadowPlugin(Shadow(vec![
                serde_json::json!({"type":"function","function":{"name":"same"}})
            ])))
            .is_err()
    );
}

struct NamedTool {
    definitions: Vec<serde_json::Value>,
}

impl Handler for NamedTool {
    fn definitions(&self) -> &[serde_json::Value] {
        &self.definitions
    }
    fn execute<'a>(&'a mut self, _: &'a ToolCall) -> PluginFuture<'a, ToolOutcome> {
        Box::pin(async { ToolOutcome::new(String::new(), true) })
    }
}

struct ToolPlugin {
    id: &'static str,
    tool: &'static str,
}

#[test]
fn a_tool_filter_applies_across_independent_handlers() {
    let mut registry = Registry::new();
    registry.install(ToolPlugin { id: "alpha", tool: "read" }).unwrap();
    registry.install(ToolPlugin { id: "beta", tool: "web_fetch" }).unwrap();
    registry.allow_tools(&["read".into()]).unwrap();
    registry.resolve().unwrap();
    assert_eq!(registry.definitions().len(), 1);
    assert_eq!(registry.definitions()[0]["function"]["name"], "read");
}

#[test]
fn a_tool_filter_is_independent_of_declaration_order() {
    let build = |filter_first: bool| {
        let mut registry = Registry::new();
        if filter_first {
            registry.allow_tools(&["read".into()]).unwrap();
        }
        registry.install(ToolPlugin { id: "alpha", tool: "read" }).unwrap();
        registry.install(ToolPlugin { id: "beta", tool: "web_fetch" }).unwrap();
        if !filter_first {
            registry.allow_tools(&["read".into()]).unwrap();
        }
        registry.resolve().unwrap();
        (registry.manifest(), registry.definitions().to_vec())
    };
    assert_eq!(build(true), build(false));
}

#[test]
fn a_tool_filter_rejects_names_no_component_provides() {
    let mut registry = Registry::new();
    registry.install(ToolPlugin { id: "alpha", tool: "read" }).unwrap();
    registry.allow_tools(&["raed".into()]).unwrap();
    let error = registry.resolve().unwrap_err().to_string();
    assert!(error.contains("unknown tool") && error.contains("raed"), "{error}");
}

impl Plugin for ToolPlugin {
    fn id(&self) -> &'static str {
        self.id
    }
    fn declare(self, registrar: &mut Registrar) -> Result<()> {
        registrar.provide_handler(NamedTool {
            definitions: vec![serde_json::json!({"type":"function","function":{"name":self.tool}})],
        })
    }
}

struct GreetingPlugin;
impl Plugin for GreetingPlugin {
    fn id(&self) -> &'static str {
        "greeting"
    }
    fn declare(self, registrar: &mut Registrar) -> Result<()> {
        registrar.provide_service::<dyn Greeting>(Rc::new(Hello));
        Ok(())
    }
}

#[test]
fn independent_plugins_obey_the_permutation_law() {
    let build = |reverse: bool| {
        let mut registry = Registry::new();
        if reverse {
            registry.install(ToolPlugin { id: "z-plugin", tool: "z_tool" }).unwrap();
            registry.install(GreetingPlugin).unwrap();
        } else {
            registry.install(GreetingPlugin).unwrap();
            registry.install(ToolPlugin { id: "z-plugin", tool: "z_tool" }).unwrap();
        }
        registry.resolve().unwrap();
        registry.manifest()
    };
    assert_eq!(build(false), build(true));
}

#[test]
fn a_rejected_contribution_is_atomic() {
    struct FailsAfterService;
    impl Plugin for FailsAfterService {
        fn id(&self) -> &'static str {
            "failing"
        }
        fn declare(self, registrar: &mut Registrar) -> Result<()> {
            registrar.provide_service::<dyn Greeting>(Rc::new(Hello));
            registrar.provide_handler(NamedTool {
                definitions: vec![serde_json::json!({"type":"function","function":{"name":"same"}})],
            })
        }
    }

    let mut registry = Registry::new();
    registry.install(ToolPlugin { id: "first", tool: "same" }).unwrap();
    assert!(registry.install(FailsAfterService).is_err());
    registry.resolve().unwrap();
    assert!(registry.services().unwrap().all::<dyn Greeting>().is_empty());
    assert_eq!(registry.installed(), &["first"]);
}

struct OrderedMiddleware(&'static str);

impl Middleware for OrderedMiddleware {
    fn on<'a>(&'a mut self, _: MiddlewareEvent<'a>) -> PluginFuture<'a, Result<Vec<MiddlewareEffect>>> {
        Box::pin(async move { Ok(vec![MiddlewareEffect::AddContext(self.0.into())]) })
    }
}

struct OrderedPlugin {
    plugin: &'static str,
}

impl Plugin for OrderedPlugin {
    fn id(&self) -> &'static str {
        self.plugin
    }
    fn declare(self, registrar: &mut Registrar) -> Result<()> {
        registrar.provide_middleware(OrderedMiddleware(self.plugin))
    }
}

#[tokio::test]
async fn behavior_modifiers_have_a_stable_order() {
    let mut registry = Registry::new();
    registry.install(OrderedPlugin { plugin: "beta" }).unwrap();
    registry.install(OrderedPlugin { plugin: "alpha" }).unwrap();
    registry.resolve().unwrap();
    let effects = registry.dispatch(MiddlewareEvent::UserPrompt { prompt: "" }).await.unwrap();
    let text = effects
        .into_iter()
        .map(|effect| match effect {
            MiddlewareEffect::AddContext(text) => text,
            _ => unreachable!(),
        })
        .collect::<Vec<_>>();
    assert_eq!(text, ["alpha", "beta"]);
}
