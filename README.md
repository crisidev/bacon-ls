# ![bacon](./img/logo.png) Bacon Language Server

LSP Server wrapper for the exceptional [Bacon](https://dystroy.org/bacon/) exposing
only [textDocument/diagnostic](https://microsoft.github.io/language-server-protocol/specification#textDocument_diagnostic) and [workspace/diagnostic](https://microsoft.github.io/language-server-protocol/specification#workspace_diagnostic)
capabilities.

![screenshot](./img/screenshot.png)

## How does it work

Bacon-ls reads the diagnostics location list generated 
by [Bacon's export-locations](https://dystroy.org/bacon/config/#export-locations)
settings and fills up the LSP client diagnostics.

It requires Bacon to be running alongside it to ensure 
the export locations are updated regularly.

## Installation

First, install [Bacon](https://dystroy.org/bacon/#installation) and Bacon-ls

```sh
❯❯❯ cargo install --locked bacon
❯❯❯ cargo install --locked bacon-ls
```

Configure Bacon export-locations settings with Bacon-ls export format:

```toml
[export]
enabled = true
path = ".bacon-locations"
line_format = "{kind}:{path}:{line}:{column}:{message}"
```

## IDE integration
### Neovim
