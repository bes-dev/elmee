/**
 * The whole of embedding, in one screen: a session, a turn, a tool of our own.
 *
 *     node example.ts [elmee binary] [project directory]
 */
import { Elmee } from "./elmee.ts";

const [binary = "elmee", project = "."] = process.argv.slice(2);
const tools = {
  selection: { description: "What the user has selected.", inputSchema: { type: "object" }, call: () => "the selected text" },
};

const agent = await Elmee.start({ binary, args: ["--no-session", "-m", "ollama/stub"], tools });
const session = await agent.session(project, {
  systemPrompt: { mode: "append", content: [{ type: "text", text: "Be brief." }] },
});
console.log(await session.prompt("ask the editor"));
agent.close();
