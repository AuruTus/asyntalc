I want to implement a harness framework for subagent work that can connect to any LLM API. The main agent would call it from the CLI, for example: `gntalk --session-id=1234-1234-1123-1234 --content=...`.

It should consist of a single executable that sets up two processes.

One process is a daemon that maintains the session state, persists it to disk, provides sandbox isolation, and exposes some tool-calling functions.

The other is a client that accepts CLI arguments, including the session ID and the text to send. It handles the input and communicates with the daemon. Once the current turn of the conversation ends, the client process returns the full text result and closes, so the main agent can use it like any other stream tool on each turn.

I mainly want to design it in Rust, but I'm not sure how difficult it will be or how long it will take to implement. The most challenging parts are probably the sandbox and tool calling. The sandbox could use Docker for now.

However, I don't know whether Rust has an ecosystem of harness tools comparable to those available for Python and Node.js. Building that ecosystem may take a lot of time and limit adoption.

I hope you can provide some feedback. As an agent or LLM, would a single-use subagent tool be a good way to cooperate with other agents? I also worry that this simple CLI invocation could block the main agent's process. In addition, it is hard to think of a seamless way to let Codex or Claude use this tool concurrently.
