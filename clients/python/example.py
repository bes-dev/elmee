"""The whole of embedding, in one screen: a session, a turn, a tool of our own.

    python3 example.py [elmee binary] [project directory]
"""

import sys

from elmee import Elmee

binary, project = (sys.argv + ["elmee", "."])[1:3]
tools = {"selection": ("What the user has selected.", {"type": "object"}, lambda arguments: "the selected text")}

with Elmee("--no-session", "-m", "ollama/stub", binary=binary, tools=tools) as agent:
    session = agent.session(project, systemPrompt={"mode": "append", "content": [{"type": "text", "text": "Be brief."}]})
    print(session.prompt("ask the editor"))
