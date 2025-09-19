# 🐽 Bacon Language Server 🐽

[![Ci](https://img.shields.io/github/actions/workflow/status/crisidev/bacon-ls/ci.yml?style=for-the-badge)](https://github.com/crisidev/bacon-ls/actions?query=workflow%3Aci)
[![Release](https://img.shields.io/github/actions/workflow/status/crisidev/bacon-ls/release.yml?style=for-the-badge)](https://github.com/crisidev/bacon-ls/actions?query=workflow%3Arelease)
[![Crates.io](https://img.shields.io/crates/v/bacon-ls?style=for-the-badge)](https://crates.io/crates/bacon-ls)
[![Crates.io](https://img.shields.io/crates/d/bacon-ls?style=for-the-badge)](https://crates.io/crates/bacon-ls)
[![License](https://img.shields.io/badge/license-MIT-blue?style=for-the-badge)](https://github.com/crisidev/bacon-ls/blob/main/LICENSE)
[![Codecov](https://img.shields.io/codecov/c/github/crisidev/bacon-ls?style=for-the-badge&token=42UR7SSSPB)](https://codecov.io/github/crisidev/bacon-ls)

**Are you tired of [rust-analyzer](https://rust-analyzer.github.io/) diagnostics being slow?**

LSP Server wrapper for the exceptional [Bacon](https://dystroy.org/bacon/) exposing [textDocument/diagnostic](https://microsoft.github.io/language-server-protocol/specification#textDocument_diagnostic) and [workspace/diagnostic](https://microsoft.github.io/language-server-protocol/specification#workspace_diagnostic) capabilities.

`bacon-ls` 🐽 does not substitute `rust-analyzer`, it's a companion tool that can help with large 
codebases where `rust-analyzer` can become slow dealing with diagnostics. 

**`bacon-ls` 🐽 does not help with completion, analysis, refactor, etc... For these, `rust-analyzer` must be running.**

![Bacon screenshot](./img/screenshot.png)

<!-- vim-markdown-toc Marked -->

* [Features](#features)
    * [Limitations](#limitations)
* [Installation](#installation)
    * [VSCode](#vscode)
    * [Mason.nvim](#mason.nvim)
    * [Manual](#manual)
    * [Nix](#nix)
* [Configuration - Native Cargo Backend](#configuration---native-cargo-backend)
* [Configuration - Bacon Backend](#configuration---bacon-backend)
    * [Neovim - LazyVim](#neovim---lazyvim)
    * [Neovim - Manual](#neovim---manual)
    * [VSCode](#vscode)
    * [Coc.nvim](#coc.nvim)
    * [Helix](#helix)
* [Troubleshooting](#troubleshooting)
    * [Bacon preferences](#bacon-preferences)
    * [Vim - Neovim](#vim---neovim)
    * [VSCode](#vscode)
* [How does it work?](#how-does-it-work?)
* [Thanks](#thanks)
* [Roadmap to 1.0 - ✅ done 🕖 in progress 🌍 future](#roadmap-to-1.0---✅-done-🕖-in-progress-🌍-future)

<!-- vim-markdown-toc -->

See `bacon-ls` 🐽 blog post: https://lmno.lol/crisidev/bacon-language-server

`bacon-ls` 🐽 is meant to be easy to include in your IDE configuration.

![Bacon gif](./img/bacon-ls.gif)

## Features

* Read diagnostics from produced by Bacon.
* Push diagnostics to the LSP client on certain events like saving or files changes.
* Precise diagnostics positions.
* Ability to react to changes over document saves and changes that can be configured.
* Replacement code actions as suggested by `clippy`.
* Automatic validation of `bacon` preferences to ensure `bacon-ls` can work with them.
* Start `bacon` in background based on user preferences (requires `bacon` 3.8.0).
* Synchronize diagnostics for all open files. 
* Support [cargo workspaces](https://doc.rust-lang.org/book/ch14-03-cargo-workspaces.html).

### Limitations

* Windows support is not tested and probably broken - [#10](https://github.com/crisidev/bacon-ls/issues/10)

## Installation

### VSCode

First, install [Bacon](https://dystroy.org/bacon/#installation).

The VSCode extension is available on both VSCE and OVSX:

* `VSCE` [https://marketplace.visualstudio.com/items?itemName=MatteoBigoi.bacon-ls-vscode](https://marketplace.visualstudio.com/items?itemName=MatteoBigoi.bacon-ls-vscode)
* `OVSX` [https://open-vsx.org/extension/MatteoBigoi/bacon-ls-vscode](https://open-vsx.org/extension/MatteoBigoi/bacon-ls-vscode)

### Mason.nvim

Both Bacon and Bacon-ls are installable via [mason.nvim](https://github.com/williamboman/mason.nvim):

```vim
:MasonInstall bacon bacon-ls
```

### Manual

First, install [Bacon](https://dystroy.org/bacon/#installation) and `bacon-ls` 🐽

```bash
❯❯❯ cargo install --locked bacon bacon-ls
❯❯❯ bacon --version
bacon 3.8.0  # make sure you have at least 3.8.0
❯❯❯ bacon-ls --version
0.14.0        # make sure you have at least 0.14.0
```

### Nix

Both [bacon](https://github.com/Canop/bacon/blob/main/flake.nix) and [bacon-ls](./flake.nix) can be consumed from their Nix flakes.

## Configuration - Native Cargo Backend

This backend will just run `cargo` with json diagnostics enabled, parse them and update diagnostics.

**NOTE: from `bacon-ls` v0.23.0, this is the default backend because it is faster and lighter than running `bacon`.

## Configuration - Bacon Backend

**NOTE: This works only with `bacon-ls` is configured with `initOptions = { useBaconBackend = true }`**

Configure Bacon export settings with `bacon-ls` 🐽 export format and proper span support in the `bacon` preference file.
To find where the file should be saved, you can use the command `bacon --prefs`:

```toml
[jobs.bacon-ls]
command = [
  "cargo", "clippy",
  "--workspace", "--all-targets", "--all-features",
  "--message-format", "json-diagnostic-rendered-ansi",
]
analyzer = "cargo_json"
need_stdout = true

[exports.cargo-json-spans]
auto = true
exporter = "analyzer"
line_format = """\
  {diagnostic.level}|:|{span.file_name}|:|{span.line_start}|:|{span.line_end}|:|\
  {span.column_start}|:|{span.column_end}|:|{diagnostic.message}|:|{diagnostic.rendered}|:|\
  {span.suggested_replacement}\
"""
path = ".bacon-locations"
```

**NOTE: `bacon` MUST be running to generate the export locations with the `bacon-ls` job: `bacon -j bacon-ls`.
From `bacon-ls` 0.10.0, this is done automatically if the option `runBaconInBackground` is set to true.**

The language server can be configured using the appropriate LSP protocol and
supports the following values:

- `locationsFile` Bacon export filename (default: `.bacon-locations`).
- `updateOnSave` Try to update diagnostics every time the file is saved (default: true).
- `updateOnSaveWaitMillis` How many milliseconds to wait before updating diagnostics after a save (default: 1000).
- `useBaconBackend` if `true`, the backend spawning `bacon` will be used instead of the native (faster) Cargo backend.
- `validateBaconPreferences`: Try to validate that `bacon` preferences are setup correctly to work with `bacon-ls` (default: true).
- `createBaconPreferencesFile`: If no `bacon` preferences file is found, create a new preferences file with the `bacon-ls` job definition (default: true).
- `runBaconInBackground`: Run `bacon` in background for the `bacon-ls` job (default: true)
- `runBaconInBackgroundCommand`: Path to the command used to run `bacon` in the background (defaults to find in `$PATH`).
- `runBaconInBackgroundCommandArguments`: Command line arguments to pass to `bacon` running in background (default "--headless -j bacon-ls")
- `synchronizeAllOpenFilesWaitMillis`: How many milliseconds to wait between background diagnostics check to synchronize all open files (default: 2000).

### Neovim - LazyVim

```lua
vim.g.lazyvim_rust_diagnostics = "bacon-ls"
```

### Neovim - Manual

NeoVim requires [nvim-lspconfig](https://github.com/neovim/nvim-lspconfig/) to be configured
and [rust-analyzer](https://rust-analyzer.github.io/) diagnostics must be turned off for `bacon-ls` 🐽
to properly function.

`bacon-ls` is part of `nvim-lspconfig` from commit
[6d2ae9f](https://github.com/neovim/nvim-lspconfig/commit/6d2ae9fdc3111a6e8fd5db2467aca11737195a30)
and it can be configured like any other LSP server works best when
[vim.diagnostics.opts.update_in_insert](https://neovim.io/doc/user/diagnostic.html#vim.diagnostic.Opts)
is set to `true`.

```lua
require("lspconfig").bacon_ls.setup({
    init_options = {
        updateOnSave = true 
        updateOnSaveWaitMillis = 1000
        ...
    }
})
```

For `rust-analyzer`, these 2 options must be turned off:

```lua
rust-analyzer.checkOnSave.enable = false
rust-analyzer.diagnostics.enable = false
```

### VSCode

The extension can be configured using the VSCode settings interface.

**It is very important that rust-analyzer `Check On Save` and `Diagnostics` are turned off for `bacon-ls` to work properly:**

* Untick `Rust-analyzer -> general -> Check On Save`
* Untick `Rust-analyzer -> diagnostics -> Enable`

### Coc.nvim

```vim
call coc#config('languageserver', {
      \ 'bacon-ls': {
      \   'command': '~/.cargo/bin/bacon-ls',
      \   'filetypes': ['rust'],
      \   'rootPatterns': ['.git/', 'Cargo.lock', 'Cargo.toml'],
      \   'initializationOptions': {
      \     'updateOnSave': v:true, 
      \     'updateOnSaveWaitMillis': 1000,
      \   },
      \   'settings': {}
      \ }
\ })
```

### Helix

Extend your `languages.toml` with the following:

```toml
[[language]]
name = "rust"
language-servers = ["rust-analyzer", "bacon-ls"]

[language-server.rust-analyzer.config]
checkOnSave = { enable = false }
diagnostics = { enable = false }

[language-server.bacon-ls]
command = "bacon-ls"
```


## Troubleshooting

`bacon-ls` 🐽 can produce a log file in the folder where its running by exporting the `RUST_LOG` variable in the shell:

### Bacon preferences

If the `bacon` preference are not correct, an error message will be published to the LSP client, advising the user to
check the README.

### Vim - Neovim

```bash
❯❯❯ export RUST_LOG=debug
❯❯❯ nvim src/some-file.rs                 # or vim src/some-file.rs
# the variable can also be exported for the current command and not for the whole shell
❯❯❯ RUST_LOG=debug nvim src/some-file.rs  # or RUST_LOG=debug vim src/some-file.rs
❯❯❯ tail -F ./bacon-ls.log
```

### VSCode

Enable debug logging in the extension options.

```bash
❯❯❯ tail -F ./bacon-ls.log
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

## Roadmap to 1.0 - ✅ done 🕖 in progress 🌍 future

- ✅ Implement LSP server interface for `textDocument/diagnostic` and `workspace/diagnostic`
- ✅ Manual Neovim configuration
- ✅ Manual [LazyVim](https://www.lazyvim.org) configuration
- ✅ Automatic NeoVim configuration
  - ✅ Add `bacon-ls` to [nvim-lspconfig](https://github.com/neovim/nvim-lspconfig/) - https://github.com/neovim/nvim-lspconfig/pull/3160
  - ✅ Add `bacon` and `bacon-ls` to [mason.nvim](https://github.com/williamboman/mason.nvim) - https://github.com/mason-org/mason-registry/pull/5774
  - ✅ Add `bacon-ls` to LazyVim [Rust extras](https://github.com/LazyVim/LazyVim/blob/main/lua/lazyvim/plugins/extras/lang/rust.lua) - https://github.com/LazyVim/LazyVim/pull/3212
- ✅ Add compiler hints to [Bacon](https://dystroy.org/bacon/) export locations - https://github.com/Canop/bacon/pull/187 https://github.com/Canop/bacon/pull/188
- ✅ Support correct span in [Bacon](https://dystroy.org/bacon/) export locations - working from `bacon` 3.7 and `bacon-ls` 0.6.0
- ✅ VSCode extension and configuration - available on the [release](https://github.com/crisidev/bacon-ls/releases) page from 0.6.0
- ✅ VSCode extension published available on Marketplace
- ✅ Add `bacon-ls` to `bacon` website - https://github.com/Canop/bacon/pull/289
- ✅ Smarter handling of parsing the Bacon locations file
- ✅ Faster response after a save event
- ✅ Replacement code actions
- ✅ Validate `bacon` preferences and return an error to the LSP client if they are not compatible with `bacon` - working from `bacon-ls` 0.9.0
- ✅ Create `bacon` preferences file if not found on disk - working from `bacon-ls` 0.10.0
- ✅ Start `bacon` in background based on user preferences - working from `bacon-ls` 0.10.0
- ✅ Synchronize diagnostics for all open files - working from `bacon-ls` 0.10.0
- ✅ Support Helix editor - working from `bacon-ls` 0.12.0
- ✅ Nix flake support
- ✅ Support [cargo workspaces](https://doc.rust-lang.org/book/ch14-03-cargo-workspaces.html) - working from `bacon-ls` 0.14.0
- ✅ Faster native cargo backend - default from `bacon-ls` 0.23.0
- 🌍 Emacs configuration
