//! One connected server: its transport, the handshake, requests and the
//! bounded reading of what it sends back.

use super::*;

/// A server's own diagnostics, kept to a bound and read only when something
/// goes wrong. Draining continuously matters even when nobody is looking: a
/// server whose stderr pipe fills up stops.
#[derive(Clone, Default)]
pub(super) struct Diagnostics(Arc<Mutex<String>>);

impl Diagnostics {
    /// The drain runs as its own task, but its handle goes back to the server
    /// that owns the pipe: a lifetime with no owner is an orphan, even one
    /// that would have ended on its own.
    fn drain(
        &self,
        stream: impl tokio::io::AsyncRead + Unpin + Send + 'static,
    ) -> tokio::task::JoinHandle<()> {
        let kept = self.0.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stream).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let mut kept = kept.lock().expect("diagnostics lock");
                kept.push_str(&line);
                kept.push('\n');
                if kept.len() > MAX_DIAGNOSTICS {
                    let trimmed = elmee_core::text::tail(&kept, MAX_DIAGNOSTICS).to_owned();
                    *kept = trimmed;
                }
            }
        })
    }

    pub(super) fn tail(&self) -> String {
        self.0.lock().map(|kept| kept.trim_end().to_owned()).unwrap_or_default()
    }
}

pub(super) enum Transport {
    /// A child process speaking JSON-RPC over its own pipes.
    Stdio { child: Child, stdin: Option<ChildStdin>, stdout: BufReader<ChildStdout> },
    /// Streamable HTTP: one POST per message, answered either with a JSON body
    /// or with an event stream the reply arrives on.
    Http { client: reqwest::Client, url: String, headers: Vec<(String, String)>, session: Option<String> },
    /// The application that embeds the agent, asked directly. Each request
    /// says who is asking, since nothing was said once at the start.
    Channel(Channel),
}

pub(super) struct McpServer {
    pub(super) name: String,
    transport: Transport,
    next_id: u64,
    timeout: Duration,
    pub(super) diagnostics: Diagnostics,
    /// The stderr drain, owned so it ends when the server does.
    drain: Option<tokio::task::JoinHandle<()>>,
    /// Whether `tools/list` was cut off at the pagination bound.
    pub(super) truncated: bool,
    /// The version both ends settled on, which every later HTTP request has to
    /// carry.
    version: String,
    root: PathBuf,
    /// How long a hung-up server gets to exit before it is killed.
    exit_grace: Duration,
}

impl Drop for McpServer {
    fn drop(&mut self) {
        if let Some(drain) = &self.drain {
            drain.abort();
        }
        // Hang up first and give the server a moment to leave on its own, so
        // that what it started — a browser — leaves with it. A kill is what a
        // server that stays gets, and all a server ever got before.
        if let Transport::Stdio { child, stdin, .. } = &mut self.transport {
            drop(stdin.take());
            let deadline = std::time::Instant::now() + self.exit_grace;
            while child.try_wait().ok().flatten().is_none() && std::time::Instant::now() < deadline {
                std::thread::sleep(self.exit_grace / 25);
            }
        }
    }
}

impl McpServer {
    pub(super) async fn start(cwd: &Path, name: &str, config: ServerConfig) -> Result<Self> {
        let timeout = config.timeout_ms.map_or(MCP_RESPONSE_TIMEOUT, Duration::from_millis);
        let diagnostics = Diagnostics::default();
        let (transport, drain) = match (&config.command, &config.url) {
            (Some(_), Some(_)) => {
                return Err(Error::Config("an MCP server has a command and a url; it takes one".into()));
            }
            (None, None) => return Err(Error::Config("an MCP server needs a command or a url".into())),
            (Some(command), None) => spawn(cwd, command, &config, &diagnostics)?,
            (None, Some(url)) => (http(url, &config)?, None),
        };
        let mut server = Self {
            name: name.to_owned(),
            transport,
            next_id: 1,
            timeout,
            diagnostics,
            drain,
            truncated: false,
            version: MCP_PROTOCOL_VERSIONS[0].to_owned(),
            root: cwd.to_owned(),
            exit_grace: Duration::from_millis(
                std::env::var("ELMEE_MCP_EXIT_GRACE_MS")
                    .ok()
                    .and_then(|ms| ms.trim().parse().ok())
                    .unwrap_or(500),
            ),
        };
        let initialized = match server
            .request(
                "initialize",
                json!({
                    "protocolVersion": MCP_PROTOCOL_VERSIONS[0],
                    // Declared rather than empty: a server asks what the client
                    // can do before it asks the client to do it, and one that is
                    // told nothing assumes nothing.
                    "capabilities": {"roots": {"listChanged": false}},
                    "clientInfo": {"name": "elmee", "version": env!("CARGO_PKG_VERSION")}
                }),
            )
            .await
        {
            Ok(initialized) => initialized,
            // The one moment a server's own diagnostics are worth most: it has
            // just died, and "closed stdout" alone reads as elmee's fault.
            Err(error) => return Err(server.explain(error).await),
        };
        let version = initialized.get("protocolVersion").and_then(Value::as_str).unwrap_or_default();
        if !MCP_PROTOCOL_VERSIONS.contains(&version) {
            return Err(Error::Provider(format!("unsupported MCP protocol version: {version}")));
        }
        server.version = version.to_owned();
        server.notify("notifications/initialized", json!({})).await?;
        Ok(server)
    }

    /// A server on the other end of a channel: no process to start and no
    /// handshake to hold, which is what the stateless revision is for.
    pub(super) fn over(cwd: &Path, name: &str, channel: Channel) -> Self {
        Self {
            name: name.to_owned(),
            transport: Transport::Channel(channel),
            next_id: 1,
            timeout: MCP_RESPONSE_TIMEOUT,
            diagnostics: Diagnostics::default(),
            drain: None,
            truncated: false,
            version: STATELESS_VERSION.to_owned(),
            root: cwd.to_owned(),
            exit_grace: Duration::ZERO,
        }
    }

    pub(super) fn kind(&self) -> &'static str {
        match self.transport {
            Transport::Stdio { .. } => "stdio",
            Transport::Http { .. } => "http",
            Transport::Channel(_) => "client",
        }
    }

    /// What went wrong, with what the server said about it.
    pub(super) async fn explain(&self, error: Error) -> Error {
        // A server that has just died may still have a line in flight: the
        // drain is a task, and reading its buffer the instant the pipe broke
        // loses the account of what happened. Waited for rather than slept on,
        // because a fixed pause is a race that passes on a quiet machine — the
        // first version of this caught a Python traceback once and missed the
        // same one the next run, when the package had been cached.
        let mut diagnostics = self.diagnostics.tail();
        for _ in 0..SETTLE_ATTEMPTS {
            if !diagnostics.is_empty() {
                break;
            }
            tokio::time::sleep(SETTLE_INTERVAL).await;
            diagnostics = self.diagnostics.tail();
        }
        if diagnostics.is_empty() {
            // Returned unchanged rather than rebuilt: wrapping an error in its
            // own kind again is how "provider error: provider error:" happens.
            return error;
        }
        // Rebuilt from the same kind, so the reader gets one "provider error:"
        // rather than one for every hand the message passed through.
        let cause = match error {
            Error::Provider(message) => message,
            other => other.to_string(),
        };
        Error::Provider(format!(
            "{cause}\n[{} said]\n{}",
            self.name,
            elmee_core::text::clip(&diagnostics, 2048)
        ))
    }

    pub(super) async fn list_tools(&mut self) -> Result<Vec<McpTool>> {
        let mut tools = Vec::new();
        let mut cursor: Option<String> = None;
        for pages in 1.. {
            let params = cursor.as_ref().map_or_else(|| json!({}), |value| json!({"cursor": value}));
            let result = self.request("tools/list", params).await?;
            let page: ToolPage = serde_json::from_value(result)?;
            tools.extend(page.tools);
            if page.next_cursor.is_some() && page.next_cursor == cursor {
                return Err(Error::Provider("MCP tools/list repeated a pagination cursor".into()));
            }
            cursor = page.next_cursor;
            if cursor.is_none() {
                break;
            }
            // A server that never stops paginating gets what it sent so far,
            // and the report says the list was cut, so nothing vanishes silently.
            if pages == MAX_TOOL_PAGES {
                self.truncated = true;
                break;
            }
        }
        Ok(tools)
    }

    pub(super) async fn request(&mut self, method: &str, mut params: Value) -> Result<Value> {
        if let Transport::Channel(channel) = &self.transport {
            params["_meta"] = json!({
                "io.modelcontextprotocol/protocolVersion": self.version,
                "io.modelcontextprotocol/clientCapabilities": {},
                "io.modelcontextprotocol/clientInfo": {"name": "elmee", "version": env!("CARGO_PKG_VERSION")},
            });
            let asked = channel(method.to_owned(), params);
            return tokio::time::timeout(self.timeout, asked).await.map_err(|_| self.late())?;
        }
        let id = self.next_id;
        self.next_id += 1;
        let message = json!({"jsonrpc":"2.0", "id":id, "method":method, "params":params});
        match matches!(self.transport, Transport::Http { .. }) {
            true => {
                let reply = self.post(&message, true).await?;
                answer_of(reply, method)
            }
            false => {
                self.send(&message).await?;
                loop {
                    let message = self.receive().await?;
                    // Answer the server's own requests instead of dropping them,
                    // so a server waiting on a reply cannot stall the turn.
                    if let Some(server_method) = message.get("method").and_then(Value::as_str) {
                        if let Some(request_id) = message.get("id").cloned() {
                            let reply = self.answer(server_method, request_id);
                            self.send(&reply).await?;
                        }
                        continue;
                    }
                    if message.get("id").and_then(Value::as_u64) != Some(id) {
                        continue;
                    }
                    return answer_of(message, method);
                }
            }
        }
    }

    /// What elmee says when a server asks it for something.
    ///
    /// Only one question has an answer: where the work is. A filesystem server
    /// that knows the project root confines itself to it, which is the same
    /// boundary everything else here respects. The rest are refused explicitly,
    /// because a server left waiting is a turn that never ends.
    fn answer(&self, method: &str, id: Value) -> Value {
        if method == "roots/list" {
            return json!({
                "jsonrpc": "2.0", "id": id,
                "result": {"roots": [{
                    "uri": format!("file://{}", self.root.display()),
                    "name": self.root.file_name().map_or_else(String::new, |name| name.to_string_lossy().into_owned()),
                }]}
            });
        }
        json!({
            "jsonrpc": "2.0", "id": id,
            "error": {"code": -32601, "message": format!("{method} is not supported by elmee")}
        })
    }

    async fn notify(&mut self, method: &str, params: Value) -> Result<()> {
        let message = json!({"jsonrpc":"2.0", "method":method, "params":params});
        if matches!(self.transport, Transport::Http { .. }) {
            return self.post(&message, false).await.map(|_| ());
        }
        self.send(&message).await
    }

    /// One Streamable HTTP exchange: post the message, and take the reply from
    /// whichever of the two shapes the server chose to answer in.
    async fn post(&mut self, message: &Value, expect_reply: bool) -> Result<Value> {
        let (client, url, headers, session) = match &self.transport {
            Transport::Http { client, url, headers, session } => (client, url, headers, session),
            _ => return Err(Error::Provider("not an HTTP transport".into())),
        };
        let mut request = client
            .post(url)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .header("MCP-Protocol-Version", &self.version);
        for (name, value) in headers {
            request = request.header(name, value);
        }
        if let Some(session) = session {
            request = request.header("Mcp-Session-Id", session);
        }
        let response = tokio::time::timeout(self.timeout, request.json(message).send())
            .await
            .map_err(|_| self.late())?
            .or_http()?;
        // Handed out on initialize and required on everything after it.
        if let Some(issued) = response.headers().get("mcp-session-id").and_then(|value| value.to_str().ok()) {
            let issued = issued.to_owned();
            if let Transport::Http { session, .. } = &mut self.transport {
                *session = Some(issued);
            }
        }
        if !response.status().is_success() {
            let status = response.status();
            let body = read_body(response, MAX_MCP_FRAME).await.unwrap_or_default();
            return Err(Error::Provider(format!(
                "MCP server answered {status}: {}",
                elmee_core::text::clip(&String::from_utf8_lossy(&body), 512)
            )));
        }
        if !expect_reply {
            return Ok(Value::Null);
        }
        let streaming = response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.starts_with("text/event-stream"));
        let id = message.get("id").cloned().unwrap_or(Value::Null);
        if streaming {
            tokio::time::timeout(self.timeout, read_events(response, &id)).await.map_err(|_| self.late())?
        } else {
            Ok(serde_json::from_slice(&read_body(response, MAX_MCP_FRAME).await?)?)
        }
    }

    fn late(&self) -> Error {
        Error::Provider(format!("MCP server did not respond within {} seconds", self.timeout.as_secs()))
    }

    async fn send(&mut self, value: &Value) -> Result<()> {
        let Transport::Stdio { stdin: Some(stdin), .. } = &mut self.transport else {
            return Err(Error::Provider("not a stdio transport".into()));
        };
        let mut encoded = serde_json::to_vec(value)?;
        encoded.push(b'\n');
        stdin.write_all(&encoded).await?;
        stdin.flush().await?;
        Ok(())
    }

    async fn receive(&mut self) -> Result<Value> {
        let timeout = self.timeout;
        let Transport::Stdio { stdout, .. } = &mut self.transport else {
            return Err(Error::Provider("not a stdio transport".into()));
        };
        let line =
            tokio::time::timeout(timeout, read_frame(stdout, MAX_MCP_FRAME)).await.map_err(|_| {
                Error::Provider(format!("MCP server did not respond within {} seconds", timeout.as_secs()))
            })??;
        Ok(serde_json::from_slice(&line)?)
    }
}

/// A JSON-RPC reply, as either the result it carries or the error it reports.
fn answer_of(message: Value, method: &str) -> Result<Value> {
    if let Some(error) = message.get("error") {
        return Err(Error::Provider(format!("MCP {method}: {error}")));
    }
    message.get("result").cloned().ok_or_else(|| Error::Provider(format!("MCP {method} returned no result")))
}

fn spawn(
    cwd: &Path,
    command: &str,
    config: &ServerConfig,
    diagnostics: &Diagnostics,
) -> Result<(Transport, Option<tokio::task::JoinHandle<()>>)> {
    let program: OsString =
        if command.contains('/') { cwd.join(command).into_os_string() } else { command.into() };
    let mut command = Command::new(program);
    elmee_plugin_process::env::apply(command.as_std_mut());
    command.envs(config.env.iter().map(|(name, value)| (name.clone(), expand(value))));
    // Declared environment is applied after the strip, so a server that
    // genuinely needs a token gets the one its config names rather than
    // whatever happened to be in the terminal.
    let mut child = command
        .args(&config.args)
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let stdin = child.stdin.take().ok_or_else(|| Error::Provider("MCP server has no stdin".into()))?;
    let stdout = child.stdout.take().ok_or_else(|| Error::Provider("MCP server has no stdout".into()))?;
    let drain = child.stderr.take().map(|stderr| diagnostics.drain(stderr));
    Ok((Transport::Stdio { child, stdin: Some(stdin), stdout: BufReader::new(stdout) }, drain))
}

pub(super) fn http(url: &str, config: &ServerConfig) -> Result<Transport> {
    // A declaration is the user's own writing rather than the model's, so this
    // is hygiene and not a contour check: plaintext to a remote host would put
    // the tokens below on the wire in the clear.
    let parsed =
        reqwest::Url::parse(url).map_err(|error| Error::Config(format!("invalid MCP url {url}: {error}")))?;
    let local = parsed.scheme() == "http"
        && matches!(parsed.host_str(), Some("localhost" | "127.0.0.1" | "::1" | "[::1]"));
    if parsed.scheme() != "https" && !local {
        return Err(Error::Config(format!("MCP url must be https, or http on the loopback: {url}")));
    }
    Ok(Transport::Http {
        client: reqwest::Client::builder().build().or_http()?,
        url: url.to_owned(),
        headers: config.headers.iter().map(|(name, value)| (name.clone(), expand(value))).collect(),
        session: None,
    })
}

/// `${VAR}` from the environment elmee itself was given.
///
/// This is what makes the credential strip usable rather than merely
/// safe: a server that needs `GITHUB_TOKEN` names it and is handed that one,
/// instead of inheriting every secret the terminal was carrying.
pub(super) fn expand(value: &str) -> String {
    let mut expanded = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(at) = rest.find("${") {
        expanded.push_str(&rest[..at]);
        let Some(end) = rest[at..].find('}') else { break };
        let name = &rest[at + 2..at + end];
        expanded.push_str(&std::env::var(name).unwrap_or_default());
        rest = &rest[at + end + 1..];
    }
    expanded.push_str(rest);
    expanded
}

/// Read an HTTP body while enforcing the bound during, not after, I/O: a
/// remote server must not make the agent allocate without limit any more than
/// a local one may.
async fn read_body(mut response: reqwest::Response, limit: usize) -> Result<Vec<u8>> {
    let too_big = || Error::Provider(format!("MCP server sent a response larger than {limit} bytes"));
    if response.content_length().is_some_and(|length| length > limit as u64) {
        return Err(too_big());
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.or_http()? {
        if body.len() + chunk.len() > limit {
            return Err(too_big());
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// Take the reply to `id` out of an event stream, ignoring what is not it.
async fn read_events(mut response: reqwest::Response, id: &Value) -> Result<Value> {
    let mut buffer = String::new();
    loop {
        let Some(chunk) = response.chunk().await.or_http()? else {
            return Err(Error::Provider("MCP event stream ended before it answered".into()));
        };
        buffer.push_str(&String::from_utf8_lossy(&chunk));
        if buffer.len() > MAX_MCP_FRAME {
            return Err(Error::Provider(format!("MCP event stream sent more than {MAX_MCP_FRAME} bytes")));
        }
        while let Some(end) = buffer.find("\n\n").or_else(|| buffer.find("\r\n\r\n")) {
            let event = buffer[..end].to_owned();
            buffer.drain(..end + if buffer[end..].starts_with("\r\n") { 4 } else { 2 });
            let data: String = event
                .lines()
                .filter_map(|line| line.strip_prefix("data:").map(str::trim_start))
                .collect::<Vec<_>>()
                .join("\n");
            if data.is_empty() {
                continue;
            }
            let Ok(message) = serde_json::from_str::<Value>(&data) else { continue };
            if message.get("id") == Some(id) {
                return Ok(message);
            }
        }
    }
}

/// Read one JSONL frame while enforcing the bound during, not after, I/O.
pub(super) async fn read_frame(reader: &mut (impl AsyncBufRead + Unpin), limit: usize) -> Result<Vec<u8>> {
    let mut frame = Vec::new();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return if frame.is_empty() {
                Err(Error::Provider("MCP server closed stdout".into()))
            } else {
                Err(Error::Provider("MCP server closed stdout in the middle of a JSONL frame".into()))
            };
        }
        let bytes = available.iter().position(|byte| *byte == b'\n').map_or(available.len(), |at| at + 1);
        if frame.len() + bytes > limit {
            let ended = available.get(bytes.saturating_sub(1)) == Some(&b'\n');
            reader.consume(bytes);
            if !ended {
                discard_frame(reader).await?;
            }
            return Err(Error::Provider(format!("MCP server sent a JSONL frame larger than {limit} bytes")));
        }
        frame.extend_from_slice(&available[..bytes]);
        reader.consume(bytes);
        if frame.last() == Some(&b'\n') {
            return Ok(frame);
        }
    }
}

async fn discard_frame(reader: &mut (impl AsyncBufRead + Unpin)) -> Result<()> {
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return Err(Error::Provider("MCP server closed stdout in the middle of a JSONL frame".into()));
        }
        let bytes = available.iter().position(|byte| *byte == b'\n').map_or(available.len(), |at| at + 1);
        let ended = available.get(bytes.saturating_sub(1)) == Some(&b'\n');
        reader.consume(bytes);
        if ended {
            return Ok(());
        }
    }
}
