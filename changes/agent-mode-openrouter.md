### Added
- **Switch the assistant off:** Settings → Agent → Models has an **Assistant** switch. Off takes the assistant out of the top bar and the dock (keys C and F12 no longer open it). MCP and CLI agents keep working.
- **Choose the listening model:** the model that hears your sound when the assistant checks it is now on the Models page, next to the chat model and the decisions model. Each takes any OpenRouter model id.
- **See when the assistant listens:** while it records the master, a bar shows how far the recording is (in the assistant dock, on the top bar's Assistant button and on the rack's hint line, so you see it with the keyboard dock open too). It tells you when nothing sounds yet, and afterwards that the listening model is judging what it heard.

### Changed
- **The assistant needs only an OpenRouter key:** one key reaches every model. The separate DeepSeek key is no longer used; a DeepSeek key left in your config is ignored (the app tells you so where it asks for the new key), and a model setting from that time ("deepseek-chat") falls back to the default. DeepSeek's models remain available through OpenRouter.
- **New default chat model:** `anthropic/claude-haiku-5.5`. Jev still decides simple requests first, and `google/gemini-3.8-flash` still listens.
- **The assistant says what it is doing:** the line under a running request now reads "Reading your request", "Thinking", "Planning the next step · after Load sound · Warm pad" or names the tool it waits for, with the model and the seconds so far, instead of "step 2".
