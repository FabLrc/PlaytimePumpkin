# PlaytimePumpkin

A [Pumpkin](https://github.com/Pumpkin-MC/Pumpkin) server plugin that tracks player playtime.

## Features

- Tracks playtime per player across sessions
- Persists data to disk between server restarts
- `/playtime` command — shows your own total playtime

## How it works

The plugin listens to two events:

- **PlayerJoinEvent** — records the join timestamp in memory
- **PlayerLeaveEvent** — computes session duration and adds it to the player's total, then persists to disk

Playtime is stored in a tab-separated flat file (`playtime_pumpkin.db`) in the plugin data folder.

## Commands

| Command | Description | Permission |
|---|---|---|
| `/playtime` | Show your total playtime | `playtimepumpkin:command.playtime` |

## Requirements

- Pumpkin server with WASM plugin support (WIT API v0.1.0)
- Rust toolchain with `wasm32-wasip2` target

## Build

```sh
rustup target add wasm32-wasip2
cargo build --release
```

The compiled plugin will be at `target/wasm32-wasip2/release/playtime_pumpkin.wasm`.
