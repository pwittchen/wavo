# wavo

AI agent for music processing

## general idea

Agent takes the command from the user in natural language via Telegram, then uses [demix](https://github.com/pwittchen/demix) MCP server, processes the song appropriately and publishes it to the [plainsong](https://github.com/pwittchen/plainsong) music storage via REST API with provided auth token and sends confirmation to the user with the link to single song and all the songs. The plainsong and demix projects can be dockerized and AI Agent logic can be created and dockerized as well in this repo. Everything should be spinned up via docker compose.
