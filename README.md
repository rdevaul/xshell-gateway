# xshell-gateway

A multi-user chat assistant built on the [xshell](https://github.com/rdevaul/xshell)
session daemon. It is a small reimplementation of the core of
[SybilClaw](https://github.com/rdevaul/sybilclaw): one assistant with one
identity, talking to several people, each with their own memory.

**Status: scaffold.** A first working slice, not a finished product. See
[what works](#what-works) and [what is missing](#what-is-missing).

## How it works

`sybild` is one process:

```text
Telegram ──▶ gateway ──▶ session daemon (xshell) ──▶ model
                │              ├─ one session per user
                │              ├─ memory tools      (compiled in)
                │              └─ per-turn context  (compiled in)
                └─ maps each chat account to a user
```

- Each user gets one durable session on the embedded daemon. The session is
  limited to the memory tools: it has no shell and cannot read files.
- Before every turn the daemon asks the context provider for the system
  prompt: the assistant's identity (`SOUL.md`, `IDENTITY.md`, `AGENTS.md`),
  the shared memory, and that user's memory. It is rebuilt each turn and
  never stored in the conversation.
- The memory tools work out whose memory to use from the session, never from
  what the model asks for, so one user's agent cannot reach another's.

[docs/architecture.md](docs/architecture.md) has the details and the reasons.

## What works

- Telegram private chats by long polling; messages from accounts that are
  not configured users are refused.
- One conversation per user, kept across restarts.
- `memory_read`, `memory_list` and `memory_write` on a per-user memory tree,
  plus read-only shared memory.
- Identity and memory in every turn's system prompt.
- An end-to-end test of all of the above against a fake model
  (`tests/slice.rs`).

## What is missing

- Heartbeat and scheduled jobs.
- Discord, group chats, images and voice.
- Skills, and signature checks on them.
- Memory search; only whole files are read.
- Summarizing old conversation. xshell drops the oldest turns when a
  conversation outgrows the model, so long conversations forget their start
  unless the agent saved it to memory.
- Tools that need approval. A chat session has none; anything that would
  prompt is denied.
- Running as an installed service. Use `sybild run` under your own service
  manager for now.

## Build

Rust 1.95 or newer.

```sh
cargo build --release
```

The xshell crates come from the `main` branch of `rdevaul/xshell`. This
scaffold needs the per-turn context hook
([xshell#166](https://github.com/rdevaul/xshell/pull/166)); until that is
merged, build against a checkout of that branch by putting this in
`.cargo/config.toml` (ignored by git):

```toml
[patch."https://github.com/rdevaul/xshell"]
xshell-core = { path = "../xshell/crates/xshell-core" }
xshell-execution = { path = "../xshell/crates/xshell-execution" }
xshell-session = { path = "../xshell/crates/xshell-session" }
```

`Cargo.lock` is not committed yet for the same reason; it will be once the
dependency is on `main`.

## Run

1. Create a bot with [@BotFather](https://t.me/BotFather) and keep its token
   in an environment variable.
2. Copy `config.example.toml`, set the paths, the model and your users.
3. Write `SOUL.md` in the workspace: who the assistant is.
4. Check, then run:

```sh
sybild check --config sybil.toml
TELEGRAM_BOT_TOKEN=... sybild run --config sybil.toml
```

To use a model that needs an API key, set `xshell_config` to an xshell
configuration file with a matching `[models.NAME]` profile. The daemon only
sends a credential to an endpoint one of its own profiles names.

Start `sybild` from an ordinary terminal or a service manager, not from
inside an xshell session: the xshell daemon refuses connections from its own
child processes.

## License

MIT. See [LICENSE](LICENSE).
