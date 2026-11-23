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

- Each user gets one durable session on the embedded daemon, limited to
  the tool groups that user is given. By default that is memory and
  scheduling, plus web search when it is configured: no shell and no file
  access.
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
- **Web search**, through a SearXNG instance or the Brave Search API.
- **Per-user tools.** Each user's agent has only the tool groups the
  configuration gives that user, and a shell only if they are given one.
- **Heartbeats.** Every so often the assistant goes through a checklist and
  messages you only if something needs your attention.
- An end-to-end test of all of the above against a fake model
  (`tests/slice.rs`).

## What is missing

- Fetching web pages, a browser, calendar or other outside services. Web
  search returns titles and snippets, not pages.
- A confined shell. Shell access is all or nothing today; confining it to
  chosen directories is being designed in xshell.
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

## Tools and who gets them

A user's `tools` line lists the tool groups their agent has. Without it, a
user gets `memory` and `schedule`, plus `web` when `[web_search]` is
configured. Shell access is a separate setting, `shell`.

| Setting | Tools | What it reaches |
|---|---|---|
| `tools`: `memory` | `memory_read`, `memory_list`, `memory_write` | The user's own memory; shared memory read-only |
| `tools`: `schedule` | `schedule_add`, `schedule_list`, `schedule_remove` | The user's own tasks |
| `tools`: `web` | `web_search` | A search service: titles, addresses and snippets |
| `shell = "unrestricted"` | `run_shell` | **This machine**, as the account `sybild` runs under |

```toml
[[users]]
name = "rich"
telegram_id = 111111111
shell = "unrestricted"

[[users]]
name = "guest"
telegram_id = 222222222
tools = ["memory"]
```

A change takes effect on the user's next message: their session is replaced
with one that has the new tools, keeping the conversation. `sybild check`
shows what each user has.

### Web search

```toml
[web_search]
backend = "searxng"
url = "https://searx.example"
```

or

```toml
[web_search]
backend = "brave"
api_key_env = "BRAVE_API_KEY"    # the variable's name; export the key itself
```

A SearXNG instance must allow the JSON format (`search.formats` in its
`settings.yml` must include `json`), or it answers 403. Another search
service is added by implementing `SearchBackend` in `src/web.rs`.

Search results are text written by strangers. The tool labels them as such
and the assistant is told not to follow instructions in them, but that is
advice to a model, not a guarantee. This matters most for a user who also
has `shell`.

### Shell

`shell = "unrestricted"` lets a user's agent run any command on this machine
without asking first, with long time limits suited to builds and test runs.
It is meant for agentic work such as coding in a local clone of a
repository.

**Give it only to someone you trust as you trust yourself.** Everything runs
under one operating-system account, so a command can read and change
whatever that account can: every user's memory, this configuration, your
other files, and the network. The separation between users described in
[docs/architecture.md](docs/architecture.md) does not hold for a user with a
shell. Text the agent reads from outside (web results, emails, logs, files in
a repository) can try to steer it; it is told not to act on such text, but
that is advice to a model, not a guarantee. Give the account's credentials
only the reach the work needs, such as a token that can open pull requests
but not push to protected branches.

It needs one line in the xshell configuration (`xshell_config`), because the
session daemon otherwise asks before every command and nobody can answer
from chat. `sybild` refuses to start without it and says so:

```toml
# xshell.toml
[session_fabric]
max_approval = "auto"
```

Commands start in the user's memory directory; the agent uses `cd DIR && ...`
to work elsewhere. They get a filtered environment: API keys and the bot
token in `sybild`'s environment are not passed on, though a command can read
files that hold them.

**Rules and logging come from xshell, not from this gateway.** Two parts of
the xshell configuration apply to an unrestricted shell:

- **Approval policy.** `denied` rules still refuse commands:

  ```toml
  [session_fabric.approval_policy]
  denied = ["run_shell: rm *", "run_shell: git push *"]
  ```

  A deny list is a guard against mistakes, not a wall: xshell's own
  documentation lists the ways a determined command gets around one.
- **Audit log.** With auditing on, the daemon records every command the
  agent asks for, the decision, and the result, in a hash-chained, signed
  log, whether or not anyone is watching. It needs xshell's audit service,
  `xshell-auditd`, running:

  ```toml
  [audit]
  enabled = true
  required = true          # refuse to run anything that cannot be recorded
  socket = "/srv/sybil/audit/audit.sock"
  directory = "/srv/sybil/audit"
  ```

  See [xshell's audit documentation](https://github.com/rdevaul/xshell/blob/main/docs/auditing.md)
  for running the service and verifying a log. If the audit service runs
  under the same account as `sybild`, a command could tamper with the log
  files; run it under its own account for a log the agent cannot alter.

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
