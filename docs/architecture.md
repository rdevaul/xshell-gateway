# Architecture

## One process

`sybild run` starts three things in one process:

| Part | Where | What it does |
|---|---|---|
| Session daemon | `xshell_session::daemon::Daemon`, main thread | Runs agent turns, enforces each session's tools, stores conversations, owns the socket |
| Providers | `src/memory.rs`, `src/context.rs`, compiled into the daemon | The memory tools and the per-turn system prompt |
| Gateway | `src/gateway.rs`, its own thread and async runtime | Receives chat messages, gives each user a session, sends answers |

They share one value, the `Directory` (`src/directory.rs`): a map from the
daemon's session ids to users. The gateway writes it when it opens a user's
session; the providers read it when the daemon calls them.

The gateway is a thread and not a separate program for a reason. The xshell
daemon refuses connections from processes it started, and requires a control
token otherwise. A thread of the daemon's own process is allowed to connect,
and a single process means one thing to start, stop and supervise.

## A message, step by step

1. `Telegram::receive` long-polls the Bot API and returns text messages from
   private chats.
2. `Gateway::route` looks the sender's account up in the configuration. An
   unknown account gets a refusal and nothing else happens.
3. The message goes to that user's worker thread. A worker owns one
   connection to the daemon and handles one message at a time, so a user's
   turns never overlap. Different users' workers run at the same time.
4. On its first message the worker attaches the user's session, `chat-NAME`,
   or creates it: durable, visible only on this host, limited to the three
   memory tools, with the user's memory directory as its working directory.
   It records the session id in the `Directory`.
5. The worker submits the message as an agent turn.
6. The daemon calls `WorkspaceContext` for the turn's system prompt, then
   runs the turn. Memory tool calls go to `MemoryProvider`.
7. The worker reads the turn's events, keeps the model's final answer, and
   sends it to the chat in pieces of at most 4096 characters.

## Who can see what

| Data | Alice's agent | Bob's agent |
|---|---|---|
| `SOUL.md`, `IDENTITY.md`, `AGENTS.md` | In every prompt | In every prompt |
| `memory/shared/` | Read | Read |
| `memory/personal/alice/` | Read and write | Nothing |
| `memory/personal/bob/` | Nothing | Read and write |
| Alice's conversation | Its own history | Nothing |

What enforces it:

- **The user comes from the session.** The daemon passes providers the
  session id. `MemoryProvider` and `WorkspaceContext` look the user up in the
  `Directory`. No tool takes a user name, so a model cannot ask for someone
  else's memory.
- **Memory paths are checked in one place** (`Workspace::resolve`): a few
  plain name segments ending in `.md`, under the user's directory, with no
  symbolic link anywhere on the path.
- **Chat sessions have no other tools.** The session's tool selection is the
  three memory tools, enforced by the daemon. There is no shell and no file
  tool, so there is no way around the memory tools.
- **Sessions the gateway did not create have no user.** A session someone
  opens with the `xshell` client gets no assistant context and the memory
  tools refuse it.

The limit of this design: everything runs as one operating-system user. The
separation between users is enforced by this program and the daemon, not by
the kernel. A deployment that needs more would run one daemon per user under
separate accounts.

## Memory in the prompt

The context has three parts, in order: the operator's files, a short note
naming the user, and the memory notes. Memory was written by an agent, from
things people said, so it can contain text that reads like instructions. It
is placed last, under a heading that says the notes are information and not
instructions. That lowers the risk; it does not remove it. Shared memory
reaches every user's prompt, so today only the operator writes it; no tool
does.

Each file is cut at 32 KiB in the prompt, and only `MEMORY.md` is included.
Topic files are read on demand with `memory_read`.

## Failures

- **The context provider fails:** the daemon fails the turn before asking
  the model anything. The user is told something went wrong.
- **A turn fails or is cancelled:** the user gets the same generic message.
  The reason goes to the log, because it can quote paths or provider errors.
- **The daemon connection breaks:** the worker drops it and reconnects on
  the user's next message. The session is durable, so the conversation is
  still there.
- **A tool would need approval:** nobody is at a terminal, so the worker
  denies it. Chat sessions hold no such tool today.

## Next

In rough order: heartbeat and scheduled jobs (as turns the gateway submits
on a timer), skills with signature checks, Discord, images, memory search,
and summarizing compaction in xshell.
