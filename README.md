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
  limited to the memory and schedule tools: it has no shell, cannot read
  files, and cannot reach the network.
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
- Identity, memory and the current time in every turn's system prompt.
- **Scheduled tasks.** Ask the assistant to remind you of something or to do
  something later or regularly ("every weekday at 9, tell me what's on my
  list"). It keeps the task with `schedule_add`, `schedule_list` and
  `schedule_remove`; when a task is due, the assistant runs it in your
  conversation and sends you the answer.
- **Heartbeats.** Every so often the assistant goes through a checklist and
  messages you only if something needs your attention.
- An end-to-end test of all of the above against a fake model
  (`tests/slice.rs`).

## What is missing

- Web search, browsing, calendar or any other outside tool. The assistant
  can only use its memory and its schedule.
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

## Install

You need Rust 1.95 or newer ([rustup.rs](https://rustup.rs)). From a checkout
of this repository:

```sh
cargo install --locked --path .
```

That builds `sybild` and puts it in `~/.cargo/bin`, which rustup adds to your
`PATH`. To try it without installing, replace `sybild` in the commands below
with `cargo run --release --`, for example
`cargo run --release -- run --config sybil.toml`.

The xshell crates come from the `main` branch of
[rdevaul/xshell](https://github.com/rdevaul/xshell), at the commit recorded
in `Cargo.lock`. Run `cargo update` to move to a newer one.

## Run

1. Create a bot with [@BotFather](https://t.me/BotFather) and keep its
   token.
2. Copy `config.example.toml` to `sybil.toml` and set the paths, the model
   and your users.
3. Write `SOUL.md` in the workspace: who the assistant is.
4. Put the bot token, and the model's API key if it needs one, in the
   environment, then check and run:

```sh
export TELEGRAM_BOT_TOKEN=...
sybild check --config sybil.toml
sybild run --config sybil.toml
```

**Environment files.** `sybild` reads secrets from its environment, and a
variable only reaches it if it is *exported*. `source .env` on a file of
plain `NAME=value` lines sets the variables in your shell but does not
export them, so `sybild` will say the token is not set. Either write
`export NAME=value` in the file, or load it with:

```sh
set -a; source .env; set +a
```

**A model that needs an API key.** The embedded daemon only sends a
credential to an endpoint its own configuration names. Write an xshell
configuration file with a profile whose `provider`, `base_url` and
`api_key_env` match the `[model]` section exactly, and point `xshell_config`
at it:

```toml
# xshell.toml
[models.main]
provider = "openai"
base_url = "https://example.net/v1"
api_key_env = "MY_API_KEY"
```

Without it, a user's first message fails and the log says "this session host
has no model profile that sends that credential to ...".

**Where to start it.** Start `sybild` from an ordinary terminal or a service
manager, not from inside an xshell session: the xshell daemon refuses
connections from its own child processes.

## Scheduled tasks and heartbeats

Both run as turns in the user's own conversation, so the assistant has the
context of what you have talked about, and both send their answer to the
user's private chat with the bot. All times are the local time of the
machine `sybild` runs on.

**Scheduled tasks** are created by asking. The assistant turns "remind me to
call mum on Friday at five" into a one-off task, and "every Monday at 9 give
me a summary of my notes" into a repeating one (a cron expression). Ask it to
list or cancel them. Limits: 50 tasks per user, and a repeating task can run
at most every 15 minutes. Tasks are stored in `schedule/NAME.json` in the
workspace. A one-off task that came due while `sybild` was not running runs
when it starts again; a repeating task's missed runs are skipped.

**Heartbeats** are turned on by the `[heartbeat]` section of the
configuration. On each one the assistant is given a checklist and answers
either with a message for you or with `HEARTBEAT_OK`, which is not sent. The
checklist is:

- `HEARTBEAT.md` at the top of the workspace, for everyone; and
- `HEARTBEAT.md` in a user's memory, which you can ask the assistant to
  change ("add 'check the tide tables' to my heartbeat checklist").

A user with neither file gets no heartbeat, so heartbeats cost nothing until
you write a checklist. A heartbeat is skipped while the user is in the
middle of a conversation turn, and outside `active_hours`.

Each scheduled task and each heartbeat is a model turn and adds to the
conversation's history.

## License

MIT. See [LICENSE](LICENSE).
