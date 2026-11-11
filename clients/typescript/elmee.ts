/**
 * Drive elmee from TypeScript over the Agent Client Protocol.
 *
 * One process serves every session, so open as many as the application needs.
 * Node's standard library only; copy this file into a project.
 *
 *     const agent = await Elmee.start();
 *     const session = await agent.session("/path/to/project");
 *     for await (const update of session.stream("what does this project do?")) {
 *       if (update.sessionUpdate === "agent_message_chunk") process.stdout.write(update.content.text);
 *     }
 *
 * Functions become tools the model can call, served over the same connection —
 * no MCP server process, no port:
 *
 *     const agent = await Elmee.start({
 *       tools: { selection: { description: "What the user has selected.", inputSchema: { type: "object" }, call: () => editor.selection() } },
 *     });
 */
import { spawn, type ChildProcessWithoutNullStreams } from "node:child_process";
import { createInterface } from "node:readline";

export type Update = { sessionUpdate: string; sessionId: string; [field: string]: any };
export type Tool = { description: string; inputSchema: object; call: (args: any) => unknown };
export type Options = {
  binary?: string;
  /** Passed to the binary: `["-m", "ollama/qwen3"]`. */
  args?: string[];
  /** The client capabilities to declare. */
  capabilities?: object;
  tools?: Record<string, Tool>;
  /** Answers what the agent asks of the client, such as `elicitation/create`. */
  onRequest?: (method: string, params: any) => unknown;
  /** Updates no stream is listening for: a child session's, or one between turns. */
  onUpdate?: (update: Update) => void;
};

export class ElmeeError extends Error {
  code?: number;
  constructor(error: { message?: string; code?: number }) {
    super(error.message ?? "failed");
    this.code = error.code;
  }
}

export class Elmee {
  /** What the agent said it is and can do. */
  info: any;
  listeners = new Map<string, (update: Update) => void>();
  private process: ChildProcessWithoutNullStreams;
  private options: Options;
  private next = 1;
  private waiting = new Map<number, { resolve: (result: any) => void; reject: (error: Error) => void }>();

  private constructor(options: Options) {
    this.options = options;
    this.process = spawn(options.binary ?? "elmee", ["--acp", ...(options.args ?? [])], {
      stdio: ["pipe", "pipe", "inherit"],
    }) as unknown as ChildProcessWithoutNullStreams;
    createInterface({ input: this.process.stdout }).on("line", (line) => this.receive(JSON.parse(line)));
    this.process.on("exit", () => {
      for (const { reject } of this.waiting.values()) reject(new ElmeeError({ message: "the agent exited" }));
    });
  }

  static async start(options: Options = {}): Promise<Elmee> {
    const agent = new Elmee(options);
    const hello = { protocolVersion: 1, clientCapabilities: options.capabilities ?? {} };
    agent.info = await agent.call("initialize", hello);
    return agent;
  }

  /** Open a session. Options are the protocol's own: `systemPrompt`, `additionalDirectories`, `mcpServers`. */
  async session(cwd: string, options: { mcpServers?: object[]; [option: string]: unknown } = {}): Promise<Session> {
    const servers = [...(options.mcpServers ?? [])];
    if (this.options.tools) servers.push({ type: "acp", name: "host", serverId: "host" });
    const opened = await this.call("session/new", { cwd, ...options, mcpServers: servers });
    return new Session(this, opened.sessionId);
  }

  /** One request, answered. */
  call(method: string, params: object): Promise<any> {
    const id = this.next++;
    this.send({ id, method, params });
    return new Promise((resolve, reject) => this.waiting.set(id, { resolve, reject }));
  }

  close(): void {
    this.process.stdin.end();
  }

  private async receive(message: any): Promise<void> {
    if (message.method === "session/update") {
      const update = { ...message.params.update, sessionId: message.params.sessionId };
      (this.listeners.get(update.sessionId) ?? this.options.onUpdate)?.(update);
    } else if (message.method && message.id !== undefined) {
      const params = message.params ?? {};
      const result =
        message.method === "mcp/message" ? await this.tool(params) : await this.options.onRequest?.(message.method, params);
      this.send(result == null ? { id: message.id, error: { code: -32601, message: message.method } } : { id: message.id, result });
    } else if (this.waiting.has(message.id)) {
      const { resolve, reject } = this.waiting.get(message.id)!;
      this.waiting.delete(message.id);
      message.error ? reject(new ElmeeError(message.error)) : resolve(message.result);
    }
  }

  /** This process as an MCP server: the two requests a tool needs. */
  private async tool(params: any): Promise<object> {
    const tools = this.options.tools ?? {};
    const asked = params.params ?? {};
    if (params.method === "tools/list") {
      const listed = Object.entries(tools).map(([name, { description, inputSchema }]) => ({ name, description, inputSchema }));
      return { result: { tools: listed } };
    }
    if (params.method === "tools/call" && tools[asked.name]) {
      // A tool that throws is told to the model, which decides what to do next.
      const [text, isError] = await Promise.resolve(tools[asked.name].call(asked.arguments ?? {})).then(
        (value) => [String(value), false],
        (error) => [String(error), true],
      );
      return { result: { content: [{ type: "text", text }], isError } };
    }
    return { error: { code: -32601, message: params.method } };
  }

  private send(message: object): void {
    this.process.stdin.write(JSON.stringify({ jsonrpc: "2.0", ...message }) + "\n");
  }
}

export class Session {
  agent: Elmee;
  id: string;

  constructor(agent: Elmee, id: string) {
    this.agent = agent;
    this.id = id;
  }

  /** Ask, and yield each update of the turn as it arrives. Returns why the turn stopped. */
  async *stream(text: string): AsyncGenerator<Update, string> {
    const queue: Update[] = [];
    const turn: { over: boolean; result?: any; error?: Error } = { over: false };
    let wake = () => {};
    this.agent.listeners.set(this.id, (update) => (queue.push(update), wake()));
    this.call("session/prompt", { prompt: [{ type: "text", text }] })
      .then((result) => (turn.result = result), (error) => (turn.error = error))
      .finally(() => ((turn.over = true), wake()));
    try {
      while (queue.length || !turn.over) {
        if (queue.length) yield queue.shift()!;
        else await new Promise<void>((resolve) => (wake = resolve));
      }
    } finally {
      this.agent.listeners.delete(this.id);
    }
    if (turn.error) throw turn.error;
    return turn.result.stopReason;
  }

  /** Ask, and return what the agent said once the turn ends. */
  async prompt(text: string): Promise<string> {
    let said = "";
    for await (const update of this.stream(text)) {
      if (update.sessionUpdate === "agent_message_chunk") said += update.content.text ?? "";
    }
    return said;
  }

  /** Any other request about this session: `session/set_title`, `_session/history`, `_session/rewind`, `session/close`. */
  call(method: string, params: object = {}): Promise<any> {
    return this.agent.call(method, { sessionId: this.id, ...params });
  }
}
