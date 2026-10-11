# Memory in the UI — design

> **Status: draft for review (revision 3). Nothing here is implemented.** Learned memory
> (`M3-DESIGN.md`, merged in #14) shipped with no wire or UI changes, by design: its decision D12 was
> "no RPC change". That left the user unable to see, approve, remove, or even know about what is being
> remembered. This is the design pass for closing that gap. It is a working contract: each part is
> retired into `DEVELOPER_GUIDE.md`, `PLAN.md` and the crate and `tui` READMEs when the milestone that
> owns it has a green gate.
>
> Revision 2 followed two independent reviews of revision 1 — an adversarial, code-grounded one and a
> fresh-eyes user-experience one (§ 12). **Revision 3 splits the scope.** Only two things are needed
> now, because they fix shipped defects or a privacy default: hardening (§ 8) and consent (§ 7). They
> ship as **M3.1**. Everything else is assigned to the milestone whose machinery it needs (§ 2a) and
> stays here as that milestone's specification; each Part A section says where it ships.
>
> Part A states what is true today and settles the design. Part B is the implementation plan for M3.1.

---

## 0. What this is for

Learned memory persists free-form text that is later injected into prompts at `user` authority, and
sends excerpts of conversations to a model to produce it. The controls that make that acceptable (see
it, remove it, know where it goes, know when it is off or failing) exist only as an `elefante memory`
CLI, and the interactive client shows none of it. The goal: **a person using the TUI is never surprised
by what the harness remembered, sent, or spent.**

**In scope:** the wire additions and the daemon side that produces them, the TUI and headless-client
rendering, a consent step, and the hardening of already-merged code that the reviews found — delivered
across M3.1, M5, M6 and M7 (§ 2a).

**Not in scope:** the first-run flow that asks for a default agent and extractor model (specified in
`PLAN.md` § First-run setup flow; it needs the model picker, which does not exist). Live updates
*between* sessions (§ 9, D8). Any change to how notes are learned, verified or ranked.

## 1. Findings

Read from the code. Items marked † were wrong or overstated in revision 1 and are corrected here.

| # | Finding | Where |
|---|---|---|
| F1 | **A stored note can carry terminal escape sequences, and the CLI prints them.** Verification collapses whitespace but not control characters, so `ESC [ 31 m` survives into a stored statement (reproduced). `escape_capped` strips them on the prompt path, so the model is safe; every *person-facing* renderer is not. Worse, `elefante memory list` opens the database with `open_memory_reader`, which **does not check the stamp**, so a repository that ships its own `.elefante/memory.db` can put text in the `kind`, `status` and `origin` columns, which are printed raw (only `statement` is whitespace-collapsed). `char::is_control` also misses bidirectional and zero-width characters, so a review surface can be spoofed. † (The clipboard route I cited in revision 1 does not apply: OSC 52 copies *selected* text.) | `mem/learn/verify.rs`, `cli/memory.rs`, `mem/schema.rs` |
| F2 | **Both clients reject an event they do not know.** `parseNotification` sets `valid = false` for an unknown `method`; the Rust `NotificationEvent` is a closed tagged enum. There is no version field. | `tui/protocol.ts`, `rpc/notification.rs` |
| F3 | **The TS reducer drops turn-less events** (`active.turn_id !== note.turn_id` returns early). | `tui/session.ts` |
| F4 | **`View::emit` takes `turn_id` from the *active turn*, not from the caller.** † A learner event that fires while the next turn runs (extraction can take 60 s) would be tagged with that turn's id. `emit` also takes a core `OutputEvent`, so learner events need a new path. Core builds the injected context *before* it emits `TurnStarted`, so a context event emitted there would be dropped by the reducer. | `srv/server.rs`, `core/lib.rs` |
| F5 | **Snapshots carry no memory state**, and a `session.attach` without `since` always returns a snapshot. The TUI never passes `since` on startup, so **a returning TUI does not replay events**; † replay happens only on an in-process socket reconnect. | `rpc/snapshot.rs`, `srv/server.rs`, `tui/main.tsx` |
| F6 | **The injected context is invisible** (`<repo-index>`, `<repo-memory>` are ephemeral by design). | `core/lib.rs` |
| F7 | **Memory health is `eprintln` into a pipe, and is partly not reported at all.** † A rebuilt store is silent (`rebuild_memory` prints nothing), and a rebuild discards learned notes. `Learner::diagnostics().last_error` is set on failure and **never cleared on success**, so one transient failure would read as "failing" for ever. | `mem/schema.rs`, `core/learn.rs` |
| F8 | **Background spend is not in the displayed cost.** `Usage.cost` is available but `Status` sums only `turn.finished`. | `core/learn.rs`, `tui/ui/Status.tsx` |
| F9 | **`Modal` handles exactly two things** (a command approval, the API key) and `Input` has no command handling. App-level handlers fire unless an overlay is a `modal`: Esc interrupts the turn, Ctrl+R toggles reasoning, and the input stays focused unless `disabled`. | `tui/ui/*.tsx` |
| F10 | **Learning is on by default and sends excerpts to a model, unannounced.** † The default extractor is the *session's own model* (it is a different vendor only if `ELEFANTE_EXTRACT_MODEL` is set), so the new disclosure is mainly *persistence* — notes saved on this machine and replayed into later prompts — and background spend. | `srv/server.rs`, `core/learn.rs` |

Not a finding any more: † revision 1's F10 (a CLI change taking ~30 s to reach a running daemon). Every
indexer refresh folds the database's generation into the daemon's mirror (`indexer.rs:399`) and
refreshes run at each turn start and end, so the lag is about one turn.

## 2. Principles

1. **Events are hints; state is truth.** Anything the user must rely on — consent, pending count,
   whether learning is working, cumulative memory spend — is *state* in `MemoryStatus` (create, attach,
   snapshot, `memory.status`). A lost event costs a missing transcript line, never a wrong total.
2. **The daemon is the preferred writer.** Panel and CLI go through it when it is running, so the
   generation mirror advances and the client needs no database path or key. The CLI falls back to the
   database directly when no daemon is listening; both are safe under SQLite WAL, and this design does
   not claim a single writer.
3. **Nothing steals focus or keystrokes.** The only thing that interrupts is the consent question, and
   only at launch with an empty input.
4. **Show nothing when healthy.** Exceptions get a few words; routine events go where the user is
   already looking (the transcript), not into new chrome.
5. **Everything shown to a person is sanitised at the rendering boundary** (F1).
6. **Say exactly what is true.** In particular what is sent: *the text of your messages and the
   assistant's replies, not tool results* — which can include pasted code and logs.

## 2a. Scope split — what ships where

| Work | § | Ships in | Why there |
|---|---|---|---|
| `display_safe`; strip at ingest; stamp check on `memory list`; failure state clears on success | 8 | **M3.1** | Defects in merged code; no dependency on anything unbuilt |
| `LearnMode`, consent file, launch modal, `elefante memory learn on\|off`, `memory.consent`, additive `memory: { consent, extractor }` result field | 7 | **M3.1** | A privacy default; needs one method and one field, no events |
| Lenient clients (TS parser, Rust `Unknown`) | 3 | **M5** | M5 adds the first new notification event (`session.stale`); leniency ships with the first emitter |
| Full `MemoryStatus` (learning state, spend, problem vocabulary), `memory.status`, `View::emit_session`, `Store::reset_reason()`, consecutive-failure health, status-line suffix, single cost total | 4.1, 5, 6.1, D7 | **M5** | M5 owns accounting; the extractor is a cost-attribution role, and the event machinery is the same |
| `InjectedContext` (which notes went into each turn) | 5 | **M6** | The audit trail records what shaped a turn; the injected notes belong in it |
| `LearnSink`, `memory.learned`, `memory.context`, transcript lines, `◇ used N saved notes` | 4.2, 5, 6.1 | **M7** | A rendering feature; needs M5's events and M6's record |
| `memory.list/forget/approve/clear` by `cwd`; the CLI preferring the daemon | 4.3, 6.2 | **M7** | Reuses M6's `repo_root` resolution and list overlay |
| `/memory` panel, `/forget`, slash-command interception, headless stderr lines | 6 | **M7** | Same list component as the M6 session picker |
| Tombstone for forgotten notes, per-repository consent, a "recent" summary on attach | 13 | after M7 | Each needs a decision this design leaves open |

M7 depends on M5 (state, events, leniency) and M6 (`repo_root`, overlay, injected-note ids); M3.1 depends
on nothing.

---

# Part A — Design

## 3. Compatibility (decision D1) — ships in M5

Revision 1 proposed lenient clients *and* opt-in per-client feature negotiation. Review showed the
second half cannot work: `seq` is assigned once, per event, in the shared `View::emit` and stored in one
ring, and both clients treat any hole in `seq` as a gap and reattach (`tui/protocol.ts:114`;
`cli/main.rs:369`, which bails after three). Filtering a memory event out for a client that did not ask
for it leaves a hole on every turn, which would break every scripted `elefante "prompt"`. It would also
solve a problem that does not arise: a daemon with a connected client never exits, so a *new* daemon is
only started when no old client is attached; the realistic skew is a new client talking to an old
daemon, which leniency already handles.

**Decision: lenient clients only, and the new events share the existing sequence space.**

- **TS:** an unknown `method` is accepted (`valid = true`) and the reducer ignores it.
- **Rust:** `NotificationEvent` gets a **hand-written `Deserialize`** — `#[serde(other)]` was tested and
  does not work on this adjacently-tagged enum for any event that carries `params` — which parses the
  `method`, deserialises known ones as today, and returns `Unknown { method }` for the rest. `Unknown` is
  never serialised.
- **Ordering of delivery:** the lenient parsers ship in the same release as the first emitter. The
  residual risk — an old client process attached to a daemon that is replaced under it — is accepted and
  documented; it cannot arise from a normal upgrade because the old daemon outlives its clients.

| Option considered | Verdict |
|---|---|
| Lenient clients only (chosen) | Smallest; forward-compatible for M5's events too |
| Opt-in `features` per client | Rejected: breaks seq continuity (above) |
| Memory events in a separate sequence space, out of the ring | Rejected for v1: a second cursor and a second resync protocol for events that are explicitly hints |

## 4. Wire additions — M5 (4.1), M7 (4.2 events, 4.3 methods)

Additive; nothing existing changes shape. Names follow the existing `noun.verb` style. Memory events go
through the session's normal ring and `seq`, and are **never coalesced**.

### 4.1 State: `MemoryStatus`

Returned in `session.create` and `session.attach` (both result kinds, so also in snapshots), and
re-sent in the `memory.status` event whenever any field changes.

```jsonc
{
  "enabled": true,                 // memory on and the store usable
  "consent": "granted",            // "unset" | "granted" | "declined" | "off"  (off = operator switch)
  "learning": "working",           // "working" | "paused" | "failing"
  "extractor": "provider/model",   // null unless learning could run
  "notes": { "active": 12, "pending": 1 },
  "index": "ready",                // "off" | "building" | "ready"   (panel header only)
  "spend": { /* Usage: cumulative memory (extractor) spend this session, incl. cost */ },
  "problem": null                  // short plain-language reason, vocabulary below
}
```

- `learning: "paused"` means consent is `unset`; **no conversation text has been read** in that state,
  and none is retained to be read later. `"failing"` means **three consecutive** extraction failures
  (the same threshold at which the learner gives a batch up) and is cleared by the next success.
- `problem` draws on a fixed vocabulary, never the provider's raw text: `"the provider refused the
  request (HTTP 402)"`, `"no reply from the provider"`, `"the reply could not be read"`,
  `"the memory key is unavailable"`, `"memory was reset: it was not made by this installation"` (shown
  until the next turn ends). The status code is the only provider-supplied datum.
- Cumulative `spend` makes cost durable (F8): the client's displayed cost is its turn total plus
  `spend.cost`, and a snapshot restores it exactly. There is no usage *event*.
- The daemon computes and emits the status **under the session's view lock**, so two sessions' ingests
  cannot interleave a stale count after a fresh one.

### 4.2 Events

All memory events are session events: `turn_id` is `null` (emitted through a new `View::emit_session`
that does not consult the active turn — F4), except `memory.context`.

| Event | Params | When |
|---|---|---|
| `memory.status` | `MemoryStatus` | any field changed |
| `memory.learned` | `{ note: Note, change: "added" \| "promoted" \| "superseded" \| "revived", replaced?: Note }` | after an ingest; **not** for `reinforced` (the common case) |
| `memory.context` | `{ notes: [Note] }`, **carries the turn's `turn_id`** | at turn start, after `turn.started`, only when learned notes were injected |

`Note = { id, kind, status, statement, origin }`, with `statement` already through `display_safe`
(§ 8). At most 8 `memory.learned` and one `memory.status` per extraction, and at most 20 extractions per
session, bound what a burst can add to the ring and a slow client's queue.

### 4.3 Methods

Repository-level methods take `{ cwd }` rather than a `session_id`, validated exactly as
`session.create` validates it. That lets `elefante memory` use a running daemon without creating a
session, and works after the session that learned a note has closed. The daemon opens the repository's
store on demand (without starting an indexer refresh).

| Method | Params | Result |
|---|---|---|
| `memory.list` | `{ cwd, all?: bool }` | `{ notes: [Row] }`; `Row` adds `confidence` (effective), `verified`, `reinforcements` |
| `memory.forget` | `{ cwd, id }` | `{}` |
| `memory.approve` | `{ cwd, id }` | `{}` |
| `memory.clear` | `{ cwd }` | `{ removed: n }` |
| `memory.status` | `{ cwd }` | `MemoryStatus` |
| `memory.consent` | `{ answer: "learn" \| "off" }` | `MemoryStatus` — user-level, applies to every session |

New errors: `UNKNOWN_NOTE` (−32006: absent, or not a learned note) and `MEMORY_OFF` (−32007: the daemon
has memory switched off or no usable store for that repository). Each mutating method is an async
wrapper in `mem::learn` that also advances the store's generation mirror, so a `forget` takes effect on
the next retrieval and is reflected in `MemoryStatus`.

### 4.4 What is not added

No events for individual extraction attempts or failures (folded into `problem`); no streaming of the
injected block's text; no usage event (cumulative state instead); no cross-session push (D8).

## 5. Daemon and core — M5, M6, M7 (config: M3.1)

- **A sink for the learner.** `Learner` gains an optional `Arc<dyn LearnSink>`: `learned(&[ChangedNote])`,
  `usage(Usage)`, `health(Health)`. `IngestReport` carries ids only, so the learner resolves the changed
  notes once with a read before reporting. `srv` implements the sink with `View::emit_session`, which
  stores in the ring like any event.
- **Health, not last error.** The learner tracks a **consecutive-failure count** (reset on success) and
  a failure *class* (provider status / timeout / unreadable reply); `diagnostics()` keeps its richer text
  for debugging but is never put on the wire.
- **Store origin.** `Store::open_with_key` records whether it **rebuilt** the store and why, exposed as
  `Store::reset_reason()`, so "memory was reset" is reportable (F7).
- **Structured context.** `ContextSource::turn_block` returns `Option<String>`; it becomes
  `Option<InjectedContext { text, notes }>` (named to avoid the existing private `TurnContext`).
  `Session::prompt` emits an `OutputEvent::Context` **after** `TurnStarted`.
- **Consent is a daemon concern, not a `Learner` concern.** The learner stays unchanged and is driven
  directly by `core`, `mem` and credentialed tests as today; `srv` simply does not call
  `Learner::observe` until consent is granted (§ 7).
- **Config.** `DaemonConfig.learn: bool` becomes `LearnMode { Off, Ask, On }` (`ELEFANTE_LEARN` unset →
  `Ask`, `1`/`on` → `On`, `0`/`off` → `Off`; `ELEFANTE_MEMORY=0` still forces everything off). Existing
  hermetic daemon tests construct `LearnMode::On`. The consent file path is derived from
  `config.credentials.parent()`, like the memory key, so tests never touch the real home.

## 6. Clients — M7 (status suffix: M5)

### 6.1 TUI

Nothing is added to the screen while memory is healthy.

**Status line.** An exception suffix on the *existing* line, plain words so it reads with no colour:

```
sonnet-5 high | 1204 in / 311 out | cost: $0.0418                                (healthy: unchanged)
sonnet-5 high | 1204 in / 311 out | cost: $0.0418 | memory: paused  /memory     (consent unset)
sonnet-5 high | 1204 in / 311 out | cost: $0.0418 | memory: not saving  /memory (3 failures in a row)
sonnet-5 high | 1204 in / 311 out | cost: $0.0418 | memory: unavailable          (key/store problem)
```

`cost` is the single total, memory included (D7).

**Transcript lines instead of toasts (D5).** Learning fires on a cue, after ~6 turns, or after idle, and
the TUI exits by *detaching*, so a timed notice is usually missed. A learned note instead becomes a
persistent dim line in the transcript, placed after the turn that was last on screen when it arrived:

```
 Use pnpm, not npm, from now on.
 Understood. I'll use pnpm.
   ◇ remembered: This repo uses pnpm, not npm.                         /forget 14
   ◇ updated: “uses npm” → “uses pnpm”                                 /forget 15
```

No kind label, nothing for `reinforced`, no expiry, no stack. A pending note (an assistant-only claim)
produces **no line and no count in the chrome**; it is visible in the panel, marked as unconfirmed, and
it is never used by the model until the user confirms it. When learned notes were injected into a turn, a
second dim line appears under that turn: `◇ used 2 saved notes` (from `memory.context`). There is no
toggle and no file count (D6). These lines live in client state for the life of the process; a fresh
attach starts from a snapshot and does not replay them (F5) — `/memory` is the durable view.

**The `/memory` panel (D3, D4).** A `modal`-kind overlay, so App's Esc-interrupts and Ctrl+R handlers do
not fire and the prompt is disabled while it is open. It opens only when the prompt is enabled (not
mid-turn). Header names the repository and the counts; rows use words, not scores; text wraps, the list
scrolls; actions act on a **note id**, re-checked when the list refreshes.

```
 Notes for ~/Developer/elefante                                   3 saved
 ──────────────────────────────────────────────────────────────────────────
 ▸ #14  This repo uses pnpm, not npm.                       you asked
   #12  Branches are named after ticket numbers.            you said
   #11  The cache is keyed by generation.                   assistant — unconfirmed
 ──────────────────────────────────────────────────────────────────────────
 ↑↓ select · f forget · Esc close
 Notes already sent to <extractor model> are not recalled. Say it again and it may be saved again.
 To approve unconfirmed notes, see history, or clear all:  elefante memory
```

`f` shows `Forget #14? “This repo uses pnpm, not npm.”  Y / N`. Approve, history (`--all`) and clear stay
in the CLI; the panel does not count unconfirmed notes anywhere else. When memory is `paused`, the
panel's first line is the consent question's current state and how to answer it.

**Slash commands.** `Input` intercepts a line only if it is *exactly* `/memory` or `/forget <integer>`;
everything else — including `/usr/bin` and any pasted block — is a prompt, and `//x` sends `/x`. The
input's placeholder gains `/memory`. There is no `/memory clear` in the TUI.

**Reducer.** `SessionState` gains `memory: MemoryStatus | null` (null for an old daemon: the UI then says
nothing, which is correct), `memory_lines: { after_turn, change, note }[]`, and a per-turn `used_notes`.
Turn-less events are handled **before** the active-turn guard. The reducer stays pure; no clock is needed
because nothing expires.

### 6.2 Headless CLI

`StreamRenderer` prints one line to **stderr**, never to stdout and never note text: `memory: saved 1
note` after a turn that produced `memory.learned`, `memory: learning is paused (not set up)` once if
consent is unset, and a `problem` once when it appears. Only when stderr is a terminal. A one-shot
`elefante "prompt"` closes its session, whose flush may extract after the answer has printed; the line
simply appears late or not at all.

`elefante memory …` **uses a running daemon** (`memory.*` over the socket) and falls back to opening the
store directly only when none is listening. It gains `elefante memory learn on|off`, which writes the
consent file (§ 7) and tells a running daemon via `memory.consent`.

## 7. Consent (decision D2) — ships in M3.1

F10 corrected: the disclosure is about **persistence and background activity**, plus — only when
`ELEFANTE_EXTRACT_MODEL` is set — a second party. Revision 1's notice at a turn end, answered with Enter
or `d`, was withdrawn: Enter is submit in a focused input and `d` is a typed letter, so a user who had
just finished typing could consent by accident, and the question arrived after the fact.

**State.** One user-level answer in `~/.elefante/memory-consent` (path beside the credentials), holding
`{ answer, extractor }` and a keyed stamp made with the memory key. A missing, unreadable or unstamped
file is `unset`. The recorded `extractor` is the model slug the user agreed to; if the configured
extractor later differs, the state reads `unset` again.

| `ELEFANTE_LEARN` | Result |
|---|---|
| unset | Ask: consent comes from the file |
| `1` / `on` | Operator has consented (headless, CI, scripts) — no question |
| `0` / `off` | Learning off; nothing is read |

An operator switch always beats the file ("env beats config").

**The question.** A blocking modal in the TUI, **at launch with the input empty** (and on reattach),
following the precedent of the first-run key modal, when `consent` is `unset`:

```
 Remember things between sessions?

 Elefante can note lasting preferences from your chats (e.g. “use pnpm”). To do that it
 sends the text of your messages and its replies — not tool results — to
 <extractor model>, and saves short notes on this machine. You can see and delete them
 any time with /memory.

   Y  learn          N  don't          Esc  ask me later
```

`Y`/`N` persist and apply to every session at once (`memory.consent`; from M5 also a `memory.status`
event to each session). `Esc` leaves it `unset`. There is no default key. The wording says the extractor's name; if it
is the session's own model, it says so (“to the same model you're chatting with”).

**While `unset`:** `learning: "paused"`, the status line says so, and the daemon **does not call
`Learner::observe`**, so no text is read, held or retained for later — consent is not retroactive.
**Headless:** a one-shot prints, once, to a terminal stderr: `memory: learning is paused (not set up) —
run “elefante memory learn on” or set ELEFANTE_LEARN=1`. A daemon never asks anything of a client that
cannot answer, and never extracts without an answer.

**The M3.1 slice.** Consent ships before any of the events in § 4.2, so it uses state, not events. The
daemon holds one shared `Consent` value (file-backed, as above) that the gate in front of
`Learner::observe` reads, so an answer reaches every live session immediately. The only wire additions
are `memory.consent { answer }` → `{ consent }` and an additive object on the `session.create` and
`session.attach` results:

```jsonc
"memory": { "consent": "unset",   // "unset" | "granted" | "declined" | "off"
            "extractor": "provider/model" }   // null unless learning could run
```

It is the seed of § 4.1's `MemoryStatus`, under the same field name; M5 grows it in place. Without a
status event, a second already-open TUI keeps its modal until it is answered or dismissed (answering is
idempotent), and there is no status-line suffix until M5 — `Esc` simply asks again at the next launch.
A client that ignores the field loses only the question; a daemon that omits it shows nothing.

**What this changes in M3's documented default:** learning was on unless `ELEFANTE_LEARN=0`; it is now
`Ask`. The credentialed live test sets `ELEFANTE_LEARN=0` for its daemon phase and drives `Learner`
directly in-process, so it is unaffected; the hermetic daemon tests move to `LearnMode::On`.

| Option considered | Verdict |
|---|---|
| Opt-out with a notice at turn end (revision 1) | Withdrawn: accidental consent, after the fact |
| Opt-in only (`ELEFANTE_LEARN=1` required) | Rejected: dormant for nearly everyone |
| Ask at first launch, persist, bind to the extractor model (chosen) | Honest, one question, headless has an operator switch |

## 8. Sanitisation (F1) — and two existing defects — ships in M3.1

One definition, `learn::display_safe(&str) -> String`, used everywhere text reaches a person or a
client:

- removes Unicode categories **Cc, Cf, Zl, Zp** (controls, bidirectional and zero-width formatting,
  line and paragraph separators), turning tab and newline into a space, and collapses whitespace;
- is applied to **every column** the CLI prints (kind, status, origin, label, statement), to every
  `Note` the daemon puts on the wire, and as defence in depth in the TUI's renderables;
- also replaces `escape_capped`'s and `flatten_capped`'s `is_control`, which miss Cf.

At ingest the same function **strips** (not rejects) control and formatting characters from the
candidate's label, statement and quote before verification — rejecting would drop a legitimate note
whenever a user pastes coloured terminal output. The quote check already ignores non-alphanumerics, so a
stripped quote still matches. Notes stored before this change are cleaned at display.

Two further defects in merged code, fixed with it: **`elefante memory list` verifies the stamp** (an
unstamped or foreign store prints a one-line explanation and no rows), and the learner **clears its
failure state on success**.

## 9. Decisions

| # | Decision | Revision 2 |
|---|---|---|
| D1 | Compatibility | Lenient clients only; events share the sequence space (§ 3) |
| D2 | Consent | Ask once at launch, persist, bind to the extractor; operator switch for headless (§ 7) |
| D3 | Raising a pending note | Panel only; no count in the chrome; never a modal |
| D4 | Panel's backend | Daemon, by `cwd`; CLI uses it when running, direct otherwise |
| D5 | Telling the user something was learned | Persistent transcript line (not a toast) |
| D6 | Telling the user what was injected | `◇ used N saved notes` line; no toggle, no file counts |
| D7 | Cost | One total including memory; breakdown in the panel header |
| D8 | Cross-session push | Not in v1: status refreshes at the next turn end, panel open, or method call |
| D9 | Forget durability | Not durable in v1 (the note can be re-learned); the panel says so. A tombstone is future work |

---

# Part B — Implementation plan

Only **M3.1** is scheduled work on this branch. It fixes defects in merged code and makes learning
ask before it reads, which needs one method and one additive field and no new events. Everything else
in Part A is real but depends on machinery later milestones build, so § 2a assigns it to them; Part A
stays as its specification and is retired into that milestone's record when it ships.

Stages are sequential, each leaves every existing test green, and each ends with a proposed commit
message for approval. No new dependencies are expected.

## M3.1 / H — Harden merged code (F1)

`learn::display_safe` (Cc/Cf/Zl/Zp); strip at ingest; apply to every column the CLI prints;
`elefante memory list` verifies the stamp; the learner clears its failure state on success.
**Tests:** a candidate with `ESC[31m`, an OSC 52 introducer, a bidi override and a zero-width space is
stored clean and still matches its quote; a *pre-existing* dirty note is displayed clean; a hand-built
unstamped database with hostile `kind`/`status`/`origin` prints nothing but the explanation; a failure
followed by a success reports healthy. Ships alone.

## M3.1 / C — Consent

- `LearnMode { Off, Ask, On }` replaces `DaemonConfig.learn: bool` (§ 5, config). Hermetic daemon tests
  construct `On`.
- The consent file and its keyed stamp (§ 7), read through a small `Consent` value the daemon shares;
  the gate in front of `Learner::observe` reads it, so an answer applies to every live session at once
  with no event.
- `memory.consent { answer }` → `{ consent }`, and an additive `memory: { consent, extractor }` object
  in the `session.create` and `session.attach` results (§ 7, "The M3.1 slice"). `rpc` does not deny
  unknown fields and the TS parser reads results by field, so an old client ignores it and a new client
  against an old daemon sees it absent and shows nothing.
- `elefante memory learn on|off`, and a one-line stderr notice from the headless client when `consent`
  is `unset`.
- The TUI launch modal (§ 7), on the existing `Modal` pattern used for the key prompt.

**Tests (stub provider, real daemon):** consent `unset` makes **zero** extractor requests (counted on the
stub) and retains nothing; `Y`/`N` persist, survive a restart, and apply to a second live session; a
changed extractor re-asks; `ELEFANTE_LEARN=1` needs no file and `0` beats a granted file; a tampered
consent file reads `unset`; an old-shaped result (no `memory`) shows no modal; `Esc` leaves it unset;
no key typed in the modal reaches the prompt; the modal never opens with text in the input.
**Docs in the same change:** `DEVELOPER_GUIDE.md` § 5 (method, result field, error), the changed
`ELEFANTE_LEARN` semantics, `MILESTONES.md`.

## 10. The M3.1 gate

1. **F1.** No note, stored or displayed, contains a control or formatting character in any column, on
   any renderer; an unstamped store prints nothing.
2. **Consent.** No extractor request is made before consent, on any client; the answer persists, is
   bound to the extractor, and is tamper-evident; `ELEFANTE_LEARN` overrides it; `paused` means no
   conversation text was read or kept.
3. **Skew.** An old client ignores the new result field; the new client against a result without it
   shows no modal; the existing daemon test suite passes unchanged.
4. **Non-blocking.** Nothing but the launch-time consent modal opens an overlay or takes focus.
5. **Quiet.** With consent granted and learning working, the screen and status line are byte-identical
   to today's.
6. **M0–M3 gates still green**, and the credentialed live test is unaffected.

### Gate items carried by later milestones

| Item | Milestone |
|---|---|
| Both clients ignore an unknown event; `seq` has no holes for any client | M5 |
| `MemoryStatus` correct for every switch and degradation, from a fresh attach and a snapshot | M5 |
| Displayed cost equals turn usage plus cumulative memory spend, and survives a reattach | M5 |
| `memory.learned` and `memory.context` carry the right `turn_id` and ordering; reinforced notes emit nothing; bursts bounded | M7 (context ids: M6) |
| `forget`/`approve`/`clear` through the daemon take effect on the next retrieval; the CLI works with and without a daemon | M7 |
| The panel never lets a key reach the prompt or interrupt a turn | M7 |

## 11. Risks

| Risk | Mitigation |
|---|---|
| An old client process meets a replaced daemon and dies on the first memory event | Accepted and documented; cannot arise from a normal upgrade (the old daemon outlives its clients); leniency ships with the first emitter |
| The hand-written `Deserialize` regresses existing parsing | Round-trip tests for every existing variant before the change; `Unknown` is deserialise-only |
| Persistent transcript lines clutter a long session | Only `added`/`promoted`/`superseded`/`revived`; the model's `reinforced` case is silent; at most one note line per change |
| Consent file lost or corrupted | Reads `unset`: extraction holds and the question reappears — fails closed |
| `Ask` as the default surprises people who relied on M3's default-on | It is the point of this design; the change is recorded in `MILESTONES.md` and `DEVELOPER_GUIDE.md`, and `ELEFANTE_LEARN=1` restores it |
| The panel and a CLI process both write | SQLite WAL with `busy_timeout`; the CLI prefers the daemon |
| Statements already in the ring or on screen cannot be recalled by a `forget` | Stated in the panel footer; out of scope to scrub the ring |

## 12. What the reviews changed

**Adversarial review** (code-grounded): showed per-client filtering breaks `seq` continuity (→ § 3, lenient
only); that consent as written contradicted the compatibility section and had no way for a headless user
to grant it (→ § 7, `LearnMode`); that `#[serde(other)]` fails on this enum (→ § 3, hand-written
`Deserialize`); that `View::emit` tags events with the active turn (→ F4, `emit_session`); that F1 was
understated, included an unstamped-database route through the CLI, and missed Cf characters (→ § 8);
that `memory.*` by `session_id` contradicted "the CLI uses the daemon" (→ § 4.3, by `cwd`); that `problem`
could not be derived as written and the rebuild is silent (→ § 4.1, § 5, F7); several TUI focus and
Esc holes (→ § 6.1); the false resume claim (→ F5); the `TurnContext` name clash; the unspecified `rpc`
typed layer; impossible byte-for-byte fixtures (→ gate 2 now asserts `seq` continuity and unchanged
tests); and my F9/F10/OSC-52 overstatements.

**User-experience review** (fresh eyes): turned toasts into transcript lines (D5); removed the permanent
status line and the pending-note nag; replaced raw provider errors with a fixed vocabulary and a
three-failure threshold; dropped Ctrl+K for a single `used N saved notes` line; folded memory cost into
one total; slimmed the panel to list/forget/close with words for provenance and a stated limit; replaced
the turn-end consent notice with a launch-time Y/N question with accurate wording; and added the CLI
`learn on|off` for headless users.

**Revision 3** (scope split): moved everything that needs events, the full `MemoryStatus`, or the list
overlay out of the first deliverable and into the milestones that build those (§ 2a), leaving hardening
and consent as M3.1. The only design change is the M3.1 consent slice in § 7, which uses state in the
create/attach result instead of events.

## 13. Open questions

1. *(after M7)* Should a fresh attach show a one-line summary of what was learned while the client was away (a
   bounded `recent` list in `MemoryStatus`)? Left out of v1 because it needs timestamps and a "since"
   marker.
2. *(after M7)* Should consent be per repository for repositories a user marks sensitive?
3. *(M3.1)* When `ELEFANTE_EXTRACT_MODEL` names a different vendor from the session's, should the question say so
   more strongly, or require a second confirmation?
4. *(after M7)* A tombstone for forgotten notes (D9): what should "never again" key on — the statement's hash, or its
   words?
5. *(M7)* Whether the headless stderr lines should be forced on or off by a flag in addition to the terminal
   check.
