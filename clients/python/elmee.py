"""Drive elmee from Python over the Agent Client Protocol.

One process serves every session, so open as many as the application needs.
Standard library only; copy this file into a project or import it from here.

    from elmee import Elmee

    with Elmee() as agent:
        session = agent.session("/path/to/project")
        for update in session.stream("what does this project do?"):
            if update["sessionUpdate"] == "agent_message_chunk":
                print(update["content"]["text"], end="")

Python functions become tools the model can call, served over the same
connection — no MCP server process, no port:

    def selection(arguments):
        return editor.selected_text()

    agent = Elmee(tools={"selection": ("What the user has selected.", {"type": "object"}, selection)})
"""

import itertools
import json
import subprocess


class ElmeeError(Exception):
    """A request the agent refused, with the protocol's own code."""

    def __init__(self, error):
        super().__init__(error.get("message", "failed"))
        self.code = error.get("code")


class Elmee:
    def __init__(self, *args, binary="elmee", capabilities=None, tools=None, on_request=None):
        """`args` go to the binary (`"-m", "ollama/qwen3"`); `capabilities` are the
        client capabilities to declare; `on_request(method, params)` answers what
        the agent asks of the client, such as `elicitation/create`."""
        self.tools = tools or {}
        self.on_request = on_request or (lambda method, params: None)
        self.process = subprocess.Popen(
            [binary, "--acp", *args], stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True
        )
        self.ids = itertools.count(1)
        self.info = self.call("initialize", {"protocolVersion": 1, "clientCapabilities": capabilities or {}})

    def __enter__(self):
        return self

    def __exit__(self, *_):
        self.close()

    def close(self):
        self.process.stdin.close()
        self.process.wait()

    def session(self, cwd, **options):
        """Open a session in `cwd`. Options are the protocol's own:
        `systemPrompt`, `additionalDirectories`, `mcpServers`."""
        servers = list(options.pop("mcpServers", []))
        if self.tools:
            servers.append({"type": "acp", "name": "host", "serverId": "host"})
        opened = self.call("session/new", {"cwd": str(cwd), "mcpServers": servers, **options})
        return Session(self, opened["sessionId"])

    def call(self, method, params, on_update=None):
        """One request, answered. Updates that arrive meanwhile go to `on_update`."""
        exchange = self.exchange(method, params)
        while True:
            try:
                update = next(exchange)
            except StopIteration as done:
                return done.value
            if on_update:
                on_update(update)

    def exchange(self, method, params):
        """One request as a generator: yields each session update, returns the result."""
        request = next(self.ids)
        self._send({"id": request, "method": method, "params": params})
        for line in self.process.stdout:
            message = json.loads(line)
            if message.get("method") == "session/update":
                yield message["params"]["update"] | {"sessionId": message["params"]["sessionId"]}
            elif "method" in message and "id" in message:
                self._answer(message)
            elif message.get("id") == request:
                if "error" in message:
                    raise ElmeeError(message["error"])
                return message["result"]
        raise ElmeeError({"message": "the agent exited"})

    def _answer(self, message):
        method, params = message["method"], message.get("params") or {}
        result = self._tool(params) if method == "mcp/message" else self.on_request(method, params)
        if result is None:
            self._send({"id": message["id"], "error": {"code": -32601, "message": method}})
        else:
            self._send({"id": message["id"], "result": result})

    def _tool(self, params):
        """This process as an MCP server: the two requests a tool needs."""
        asked = params.get("params") or {}
        if params["method"] == "tools/list":
            listed = [
                {"name": name, "description": description, "inputSchema": schema}
                for name, (description, schema, _) in self.tools.items()
            ]
            return {"result": {"tools": listed}}
        if params["method"] == "tools/call" and asked.get("name") in self.tools:
            try:
                text, failed = str(self.tools[asked["name"]][2](asked.get("arguments") or {})), False
            except Exception as error:  # the model is told, and decides what to do next
                text, failed = str(error), True
            return {"result": {"content": [{"type": "text", "text": text}], "isError": failed}}
        return {"error": {"code": -32601, "message": params["method"]}}

    def _send(self, message):
        self.process.stdin.write(json.dumps({"jsonrpc": "2.0", **message}) + "\n")
        self.process.stdin.flush()


class Session:
    def __init__(self, agent, id):
        self.agent, self.id = agent, id

    def stream(self, text):
        """Ask, and yield each update of the turn as it arrives."""
        prompt = [{"type": "text", "text": text}]
        return self.agent.exchange("session/prompt", {"sessionId": self.id, "prompt": prompt})

    def prompt(self, text):
        """Ask, and return what the agent said once the turn ends."""
        said = []
        for update in self.stream(text):
            if update["sessionUpdate"] == "agent_message_chunk" and update["sessionId"] == self.id:
                said.append(update["content"].get("text", ""))
        return "".join(said)

    def call(self, method, **params):
        """Any other request about this session: `session/set_title`,
        `_session/history`, `_session/rewind`, `session/close`."""
        return self.agent.call(method, {"sessionId": self.id, **params})
