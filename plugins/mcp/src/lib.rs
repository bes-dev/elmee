//! Tools that come from somewhere else.
//!
//! A server is declared, started, asked what it can do, and its answers are
//! offered to the model beside the built-in tools. Three transports: a child
//! process speaking JSON-RPC over its own pipes, Streamable HTTP for the
//! servers that live somewhere else, and a channel the embedding application
//! hands over for a server it is itself. Everything above the transport is
//! the same protocol, so the split stops at `Transport` and nothing else in
//! this file knows which it is.
//!
//! Two scopes. `~/.elmee/mcp.json` is the machine's own, written by the person
//! who owns the account; `.elmee/mcp.json` arrives with a checkout and is read
//! after it, so a project may add servers but may not shadow a name the user
//! has already given.
//!
//! What a server is *given* is this file's business: the environment is
//! stripped of anything credential-shaped before the process starts, so a
//! server that needs a token names it — `"env": {"GITHUB_TOKEN": "${GITHUB_TOKEN}"}`
//! — and receives that one rather than everything the terminal happened to
//! carry.

use std::{
    collections::HashMap,
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, Mutex},
    time::Duration,
};

use serde::Deserialize;
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout, Command},
};

use elmee_core::{
    Error, Handler, Plugin, PluginFuture, Registrar, Result, ToolOutcome,
    message::{Image, ToolCall},
};

trait OrHttp<T> {
    fn or_http(self) -> Result<T>;
}

impl<T> OrHttp<T> for std::result::Result<T, reqwest::Error> {
    fn or_http(self) -> Result<T> {
        self.map_err(|error| Error::Http(error.to_string()))
    }
}

const MCP_RESPONSE_TIMEOUT: Duration = Duration::from_secs(30);
/// One JSONL frame. Tool payloads may be sizeable, but a server must not make
/// the agent allocate without limit before it sends a newline.
const MAX_MCP_FRAME: usize = 4 * 1024 * 1024;
const MCP_PROTOCOL_VERSIONS: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];
/// The revision with no handshake, which is the one a channel carries.
const STATELESS_VERSION: &str = "2026-07-28";
/// The strictest provider's ceiling on a function name.
const MAX_TOOL_NAME: usize = 64;
/// A context-scale bound on what one tool result may put in the conversation.
/// The frame limit above is transport hygiene; this one is about the model's
/// attention.
const MAX_MCP_OUTPUT: usize = 64 * 1024;
/// How far to follow `tools/list` pagination before deciding the server is
/// never going to stop.
const MAX_TOOL_PAGES: usize = 16;
/// How much of a server's own diagnostics to keep for the moment it fails.
const MAX_DIAGNOSTICS: usize = 4 * 1024;
/// How long to let a dying server finish saying why, before giving up on it.
const SETTLE_INTERVAL: Duration = Duration::from_millis(25);
const SETTLE_ATTEMPTS: usize = 20;

#[derive(Default)]
pub struct McpRegistry {
    servers: Vec<McpServer>,
    routes: HashMap<String, (usize, String)>,
    definitions: Vec<Value>,
    /// What went wrong during discovery, kept so `report` can say it later:
    /// stderr under a raw-mode terminal is a message to nobody.
    warnings: Vec<String>,
}

impl McpRegistry {
    /// Connect every declared server. One that cannot be reached degrades to
    /// a warning: a broken server must not take the agent down with it.
    pub async fn discover(cwd: &Path, given: &[Declared]) -> Result<Self> {
        Self::discover_in(cwd, &scopes(cwd), given).await
    }

    async fn discover_in(cwd: &Path, scopes: &[PathBuf], given: &[Declared]) -> Result<Self> {
        let mut registry = Self::default();
        let mut claimed: Vec<String> = Vec::new();
        // What the application was handed comes first, and therefore owns its
        // names: a caller that knows which servers this piece of work needs is
        // more specific than a file that describes the machine.
        for server in given {
            claimed.push(server.name.clone());
            let connected = match &server.channel {
                Some(channel) => registry.offer(McpServer::over(cwd, &server.name, channel.clone())).await,
                None => registry.connect(cwd, &server.name, server.config()).await,
            };
            if let Err(error) = connected {
                registry.warnings.push(format!("skipping MCP server {}: {error}", server.name));
            }
        }
        for path in scopes {
            if !path.is_file() {
                continue;
            }
            let configured = match load(path) {
                Ok(configured) => configured,
                Err(error) => {
                    registry.warnings.push(format!("ignoring {}: {error}", path.display()));
                    continue;
                }
            };
            // Sorted, so the tool definitions this produces are in the same
            // order on every run: they sit at the front of every request, and
            // one byte of drift there throws away the prompt cache.
            let mut configured = configured.into_iter().collect::<Vec<_>>();
            configured.sort_unstable_by(|a, b| a.0.cmp(&b.0));
            for (name, config) in configured {
                if claimed.contains(&name) {
                    registry.warnings.push(format!(
                        "{} declares MCP server {name}, which you already own",
                        path.display()
                    ));
                    continue;
                }
                claimed.push(name.clone());
                if let Err(error) = registry.connect(cwd, &name, config).await {
                    registry.warnings.push(format!("skipping MCP server {name}: {error}"));
                }
            }
        }
        Ok(registry)
    }

    async fn connect(&mut self, cwd: &Path, server_name: &str, config: ServerConfig) -> Result<()> {
        self.offer(McpServer::start(cwd, server_name, config).await?).await
    }

    /// Ask a connected server what it can do, and offer that to the model.
    async fn offer(&mut self, mut server: McpServer) -> Result<()> {
        let server_name = &server.name.clone();
        let tools = match server.list_tools().await {
            Ok(tools) => tools,
            Err(error) => return Err(server.explain(error).await),
        };
        let server_index = self.servers.len();
        // Stage everything first so a failure cannot leave routes to an unregistered server.
        let mut definitions = Vec::new();
        let mut routes = Vec::new();
        for tool in tools {
            let exposed = exposed_name(server_name, &tool.name);
            // One colliding name used to reject the whole server, which cost
            // every other tool it offered for the sake of one.
            if self.routes.contains_key(&exposed) || routes.iter().any(|(name, _)| *name == exposed) {
                self.warnings.push(format!("{server_name} offers {} under a name already taken", tool.name));
                continue;
            }
            let description = tool.description.unwrap_or_else(|| format!("MCP tool {}", tool.name));
            let mut input_schema = tool.input_schema;
            mark_schema_descriptions(&mut input_schema);
            definitions.push(json!({
                "type": "function",
                "function": {
                    "name": exposed,
                    "description": elmee_core::text::fenced(
                        "mcp_description",
                        " trusted=\"no\"",
                        &elmee_core::text::clip(&description, 4096),
                    ),
                    "parameters": input_schema
                }
            }));
            routes.push((exposed, tool.name));
        }
        self.definitions.extend(definitions);
        self.routes.extend(routes.into_iter().map(|(exposed, name)| (exposed, (server_index, name))));
        self.servers.push(server);
        Ok(())
    }

    pub fn definitions(&self) -> &[Value] {
        &self.definitions
    }

    pub fn has(&self, name: &str) -> bool {
        self.routes.contains_key(name)
    }

    /// What is connected and what it offered, for a person who wants to know
    /// why a tool is or is not there. Until this existed the only account of a
    /// server was a line on stderr at startup, which a terminal interface eats.
    pub fn report(&self) -> String {
        let mut lines: Vec<String> = self.warnings.iter().map(|warning| format!("! {warning}")).collect();
        if self.servers.is_empty() {
            lines.push(
                "No MCP servers are connected. Declare them in ~/.elmee/mcp.json or in the \
                 project's .elmee/mcp.json."
                    .to_owned(),
            );
            return lines.join("\n");
        }
        for (index, server) in self.servers.iter().enumerate() {
            let mut tools: Vec<&str> = self
                .routes
                .iter()
                .filter(|(_, (at, _))| *at == index)
                .map(|(exposed, _)| exposed.as_str())
                .collect();
            tools.sort_unstable();
            let count = tools.len();
            let plural = if count == 1 { "tool" } else { "tools" };
            let truncated = if server.truncated { " (tool list truncated)" } else { "" };
            lines.push(format!("{} · {} · {count} {plural}{truncated}", server.name, server.kind()));
            lines.extend(tools.into_iter().map(|tool| format!("  {tool}")));
            let diagnostics = server.diagnostics.tail();
            if !diagnostics.is_empty() {
                lines.push(format!("  [{} said]", server.name));
                lines.extend(
                    elmee_core::text::tail(&diagnostics, 512).lines().map(|line| format!("  │ {line}")),
                );
            }
        }
        lines.join("\n")
    }

    pub async fn call(&mut self, name: &str, arguments: &str) -> Result<(String, Vec<Image>)> {
        let (server, original) =
            self.routes.get(name).cloned().ok_or_else(|| Error::Tool(format!("unknown MCP tool: {name}")))?;
        let arguments: Value = serde_json::from_str(arguments)?;
        let result = self.servers[server]
            .request("tools/call", json!({"name": original, "arguments": arguments}))
            .await;
        match result {
            Ok(result) => Ok((render_result(&result)?, images_of(&result))),
            // A server that has died says so once, with whatever it printed on
            // the way out; "closed stdout" alone sends the agent looking at its
            // own arguments for a fault that is not there.
            Err(error) => {
                Err(Error::Tool(format!("MCP server {name}: {}", self.servers[server].explain(error).await)))
            }
        }
    }
}

/// MCP as a tool plugin: the servers a session was given and the machine
/// declares, connected once when the agent is composed.
pub struct McpPlugin {
    registry: McpRegistry,
}

impl McpPlugin {
    /// The one construction every composition root performs: a live graph
    /// discovers its servers, and an explained or memory-only one declares the
    /// same capability inertly. Either way this is the final construction step,
    /// because discovery may start processes.
    pub async fn for_mode(live: bool, cwd: PathBuf, given: Vec<Declared>) -> Result<Self> {
        if live { Self::discover(cwd, given).await } else { Ok(Self { registry: McpRegistry::default() }) }
    }

    async fn discover(cwd: PathBuf, given: Vec<Declared>) -> Result<Self> {
        let registry = McpRegistry::discover(&cwd, &given).await?;
        // Startup runs before any frontend exists, so stderr is still a place
        // a person can see; later the same warnings live in `report`.
        for warning in &registry.warnings {
            eprintln!("elmee: {warning}");
        }
        Ok(Self { registry })
    }
}

impl Plugin for McpPlugin {
    fn id(&self) -> &'static str {
        "mcp"
    }

    fn declare(self, plugins: &mut Registrar) -> Result<()> {
        plugins.provide_handler(self)
    }
}

impl Handler for McpPlugin {
    fn definitions(&self) -> &[Value] {
        self.registry.definitions()
    }

    fn execute<'a>(&'a mut self, call: &'a ToolCall) -> PluginFuture<'a, ToolOutcome> {
        Box::pin(async move {
            match self.registry.call(&call.function.name, &call.function.arguments).await {
                Ok((output, images)) => {
                    let failed = output.starts_with("Error:");
                    let framed = elmee_core::text::fenced("mcp_output", " trusted=\"no\"", &output);
                    let mut outcome = if failed {
                        ToolOutcome::new(format!("Error: {framed}"), false)
                    } else {
                        ToolOutcome::new(framed, true)
                    };
                    outcome.images = images;
                    outcome
                }
                Err(error) => ToolOutcome::new(format!("Error: {error}"), false),
            }
        })
    }

    fn report(&self) -> String {
        self.registry.report()
    }
}

/// Where servers are declared, and whether that place speaks with the user's
/// own authority. Nearest last, so a project is read after the user and can be
/// refused the names the user has already used.
fn scopes(cwd: &Path) -> Vec<PathBuf> {
    let mut scopes = Vec::new();
    if let Some(home) = std::env::var_os("HOME") {
        scopes.push(PathBuf::from(home).join(".elmee/mcp.json"));
    }
    scopes.push(cwd.join(".elmee/mcp.json"));
    scopes
}

/// An MCP server the application was told to start, rather than one it found.
///
/// The application's own vocabulary: whoever hands these over — a command
/// line, an editor opening a session — describes a program to run, not a
/// configuration file format.
#[derive(Clone)]
pub struct Declared {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    /// A server reached over Streamable HTTP instead of started here.
    pub url: Option<String>,
    pub headers: Vec<(String, String)>,
    /// A server the application itself is, reached by asking it.
    pub channel: Option<Channel>,
}

/// One MCP request put to whoever serves it — a method and its params in, the
/// result out — for a server that is neither a process nor an address.
pub type Channel = std::rc::Rc<dyn Fn(String, Value) -> PluginFuture<'static, Result<Value>>>;

impl Declared {
    fn config(&self) -> ServerConfig {
        ServerConfig {
            command: (!self.command.is_empty()).then(|| self.command.clone()),
            args: self.args.clone(),
            env: self.env.iter().cloned().collect(),
            url: self.url.clone(),
            headers: self.headers.iter().cloned().collect(),
            timeout_ms: None,
        }
    }
}

/// Read a declaration file: the `mcpServers` map every other client writes,
/// so a configuration that already exists can be pasted rather than translated.
fn load(path: &Path) -> Result<HashMap<String, ServerConfig>> {
    #[derive(Deserialize)]
    struct File {
        #[serde(rename = "mcpServers")]
        servers: HashMap<String, ServerConfig>,
    }
    Ok(serde_json::from_slice::<File>(&fs::read(path)?)?.servers)
}

/// The name a provider sees.
///
/// Two constraints meet: it must pass the strictest provider's validation —
/// letters, digits and underscores, at most sixty-four of them — and it must be
/// the same string on every run, because a tool definition that varies un-caches
/// every request behind it. A name too long keeps its readable head and earns a
/// digest of the whole, which is stable and cannot collide by accident.
fn exposed_name(server: &str, tool: &str) -> String {
    let full = format!("mcp__{}__{}", sanitize(server), sanitize(tool));
    if full.len() <= MAX_TOOL_NAME {
        return full;
    }
    let digest = ring::digest::digest(&ring::digest::SHA256, full.as_bytes());
    let suffix: String = digest.as_ref()[..3].iter().map(|byte| format!("{byte:02x}")).collect();
    format!("{}_{suffix}", &full[..MAX_TOOL_NAME - suffix.len() - 1])
}

mod server;
use server::*;

#[derive(Deserialize)]
struct ServerConfig {
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: HashMap<String, String>,
    /// A server that lives somewhere else, spoken to over Streamable HTTP.
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    headers: HashMap<String, String>,
    #[serde(default, rename = "timeoutMs")]
    timeout_ms: Option<u64>,
}

#[derive(Deserialize)]
struct ToolPage {
    tools: Vec<McpTool>,
    #[serde(rename = "nextCursor")]
    next_cursor: Option<String>,
}

#[derive(Deserialize)]
struct McpTool {
    name: String,
    description: Option<String>,
    #[serde(rename = "inputSchema")]
    input_schema: Value,
}

fn mark_schema_descriptions(value: &mut Value) {
    match value {
        Value::Object(fields) => {
            if let Some(Value::String(description)) = fields.get_mut("description") {
                *description = elmee_core::text::fenced(
                    "mcp_description",
                    " trusted=\"no\"",
                    &elmee_core::text::clip(description, 4096),
                );
            }
            for child in fields.values_mut() {
                mark_schema_descriptions(child);
            }
        }
        Value::Array(items) => items.iter_mut().for_each(mark_schema_descriptions),
        _ => {}
    }
}

fn sanitize(name: &str) -> String {
    name.chars().map(|character| if character.is_ascii_alphanumeric() { character } else { '_' }).collect()
}

/// The pictures in a result, for the model to look at.
fn images_of(result: &Value) -> Vec<Image> {
    result["content"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|block| block["type"] == "image")
        .filter_map(|block| {
            Some(Image {
                media_type: block["mimeType"].as_str()?.to_owned(),
                data: block["data"].as_str()?.to_owned(),
            })
        })
        .collect()
}

fn render_result(result: &Value) -> Result<String> {
    let mut output = String::new();
    if result.get("isError").and_then(Value::as_bool) == Some(true) {
        output.push_str("Error: ");
    }
    if let Some(content) = result.get("content").and_then(Value::as_array) {
        for block in content {
            if !output.is_empty() && !output.ends_with(' ') {
                output.push('\n');
            }
            match block.get("type").and_then(Value::as_str) {
                Some("text") => {
                    output.push_str(block.get("text").and_then(Value::as_str).unwrap_or_default())
                }
                // The picture itself follows the results; here is its place.
                Some("image") => output.push_str(&format!(
                    "[image {} returned by the tool; shown after the results]",
                    block.get("mimeType").and_then(Value::as_str).unwrap_or("of unknown type")
                )),
                Some("resource") => match block.get("resource").and_then(|resource| resource.get("text")) {
                    Some(Value::String(text)) => output.push_str(text),
                    _ => output.push_str(&serde_json::to_string(block)?),
                },
                _ => output.push_str(&serde_json::to_string(block)?),
            }
        }
    }
    if output.is_empty()
        && let Some(structured) = result.get("structuredContent")
    {
        output = serde_json::to_string(structured)?;
    }
    // The frame limit is transport hygiene; this is the context boundary. A
    // cooperatively verbose server otherwise puts megabytes into one message.
    Ok(elmee_core::text::bound(output, MAX_MCP_OUTPUT, true))
}

#[cfg(test)]
mod tests;
