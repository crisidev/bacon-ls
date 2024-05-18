# 🐽 Bacon Language Server 🐽
[![Ci](https://img.shields.io/github/actions/workflow/status/crisidev/bacon-ls/test.yml?style=for-the-badge)](https://github.com/crisidev/bacon-ls/actions?query=workflow%3Atest)
[![Crates.io](https://img.shields.io/crates/v/bacon-ls?style=for-the-badge)](https://crates.io/crates/bacon-ls)
[![License](https://img.shields.io/badge/license-MIT-blue?style=for-the-badge)](https://github.com/crisidev/bacon-ls/blob/main/LICENSE)

LSP Server wrapper for the exceptional [Bacon](https://dystroy.org/bacon/) exposing [textDocument/diagnostic](https://microsoft.github.io/language-server-protocol/specification#textDocument_diagnostic) and [workspace/diagnostic](https://microsoft.github.io/language-server-protocol/specification#workspace_diagnostic) capabilities.

See `bacon-ls` 🐽 blog post: https://lmno.lol/crisidev/bacon-language-server

![Bacon screenshot](./screenshot.png)

`bacon-ls` 🐽 is meant to be easy to include in your IDE configuration.

<!-- vim-markdown-toc Marked -->

* [Features - ✅ done 🕖 in progress 🌍 future](#features---✅-done-🕖-in-progress-🌍-future)
* [Installation](#installation)
* [Configuration](#configuration)
    * [Neovim - Manual](#neovim---manual)
    * [Neovim - LazyVim](#neovim---lazyvim)
* [How does it work?](#how-does-it-work?)
* [Thanks](#thanks)

<!-- vim-markdown-toc -->

## Features - ✅ done 🕖 in progress 🌍 future

- ✅ Implement LSP server interface for `textDocument/diagnostic` and `workspace/diagnostic` 
- ✅ Manual Neovim configuration
- ✅ Manual [LazyVim](https://www.lazyvim.org) configuration
- 🕖 Automatic NeoVim configuration
    - 🕖 Add `bacon-ls` to [nvim-lspconfig](https://github.com/neovim/nvim-lspconfig/)
    - 🕖 Add `bacon-ls` to [mason.nvim](https://github.com/williamboman/mason.nvim) 
    - 🕖 Add bacon-ls to LazyVim [Rust extras](https://github.com/LazyVim/LazyVim/blob/main/lua/lazyvim/plugins/extras/lang/rust.lua)
- 🕖 Add compiler hints to [Bacon](https://dystroy.org/bacon/) export locations
- 🌍 VsCode extension and configuration
- 🌍 Emacs configuration

![Bacon gif](./bacon-ls.gif)
## Installation

First, install [Bacon](https://dystroy.org/bacon/#installation) and `bacon-ls` 🐽

```bash
❯❯❯ cargo install --locked bacon bacon-ls
```

Configure Bacon export-locations settings with `bacon-ls` 🐽 export format:

```toml
[export]
enabled = true
path = ".bacon-locations"
line_format = "{kind}:{path}:{line}:{column}:{message}"
```

## Configuration
The language server can be configured using the appropriate LSP protocol and
supports the following values:

* `locationsFile` Bacon export filename, default `.bacon-locations`.
* `waitTimeSeconds` Maximum time in seconds the LSP server waits for Bacon to 
update the export file before loading the new diagnostics, default `10`.
### Neovim - Manual

NeoVim requires [nvim-lspconfig](https://github.com/neovim/nvim-lspconfig/) to be configured 
and [rust-analyzer](https://rust-analyzer.github.io/) diagnostics must be turned off for Bacon-Ls 🐽 
to properly function.

`nvim-lspconfig` must be configured to start bacon-ls 🐽 when opening
the first Rust 🦀 file and works best when `update_in_insert = true`
is set.

```lua
local configs = require("lspconfig.configs")
if not configs.bacon_ls then
    configs.bacon_ls = {
        default_config = {
            cmd = { "bacon-ls" },
            root_dir = require("lspconfig").util.root_pattern(".git"),
            filetypes = { "rust" },
            settings = {
                -- locationsFile = ".locations",
                -- waitTimeSeconds = 5
            }
        },
    }
end
lspconfig.bacon_ls.setup({ autostart = true })
```

For `rust-analyzer`, these 2 options must be turned off:

```lua
rust-analyzer.checkOnSave.enable = false
rust-analyzer.diagnostics.enable = false
```

### Neovim - LazyVim 
```lua
return {
    {
        "neovim/nvim-lspconfig",
        opts = {
            diagnostics = {
                update_in_insert = true,
            },
            servers = {
                rust_analyzer = { enable = false },
                bacon_ls = { enable = true },
            },
            setup = {
                bacon_ls = function()
                    local configs = require("lspconfig.configs")
                    if not configs.bacon_ls then
                        configs.bacon_ls = {
                            default_config = {
                                cmd = { "bacon-ls" },
                                root_dir = require("lspconfig").util.root_pattern(".git"),
                                filetypes = { "rust" },
                                settings = {
                                    -- locationsFile = ".locations",
                                    -- waitTimeSeconds = 5
                                }
                            },
                        }
                    end
                    lspconfig.bacon_ls.setup({})
                    return true
                end,
            },
        },
    },
    {
        "mrcjkb/rustaceanvim",
        opts = {
            default_settings = {
                ["rust-analyzer"] = { 
                    diagnostics = { enable = false },
                    checkOnSave = { enable = false },
                }
            }
        }
    }
}
```
## How does it work?

`bacon-ls` 🐽 reads the diagnostics location list generated
by [Bacon's export-locations](https://dystroy.org/bacon/config/#export-locations) 
and exposes them on STDIO over the LSP protocol to be consumed
by the client diagnostics.

It requires [Bacon](https://dystroy.org/bacon/) to be running alongside 
to ensure regular updates of the export locations.

The LSP client reads them as response to `textDocument/diagnostic` and `workspace/diagnostic`.

## Thanks
`bacon-ls` 🐽 has been inspired by [typos-lsp](https://github.com/tekumara/typos-lsp).
