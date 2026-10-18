# Dreams

Give your agents a subconscious. Dreams is a memory store that lets agents learn while they sleep. Capture notes and agent memories. Background agents dream over them, uncovering new insights.

You write Markdown. Your agents write Markdown too, or plain facts. Both are stored as `{the, of, is}` triples, so an agent can ask the vault questions in Datalog ("which notes link to a note that does not exist yet?") as well as search it. Scheduled agents wake on a timer, or when the answer to a query changes. The vault syncs between your devices through a folder, S3, or a UCAN service, and you can follow other people's published notes.

It is one Rust binary. There is no server, and your data stays on your disk and in the remotes you choose.

> **Status: experimental.** Dreams is built on [DialogDB](https://github.com/dialog-db/dialog-db) 0.4.0-rc.2, which changes most days and promises no migrations. The database format can change between builds of dreams. Keep an export (`dreams export`) of anything you care about.

## Build

```
cargo build --release
```

The binary is `target/release/dreams`. You need a Rust toolchain that supports edition 2024. Dialog is not on crates.io, so the first build fetches it from GitHub, pinned to the tag `v0.4.0-rc.2`.

## Quick start

```
dreams init

dreams note put notes/sleep.md <<'EOF'
---
title: Sleep
tags: [health, habits]
---
Naps after 3pm hurt my sleep. See [[notes/caffeine.md]].
EOF

dreams note list
dreams note search naps
dreams note get notes/sleep.md
```

The vault is the folder `~/.dreams`. Pass `--vault DIR` or set `DREAMS_VAULT` to use another one. `init` creates the vault and seeds the built-in runners, skills, prompts, and tasks. Any other command also creates the vault on first use.

`note get` prints the note with every fact about it:

```
---
dreams.note:
  by: did:key:z6MksY6f…
  created: 1792316677225
  dangling:
  - notes/caffeine.md
  path: notes/sleep.md
  tag:
  - habits
  - health
  title: Sleep
  updated: 1792316677226
entity: did:key:z6Mk6zmj…
---
Naps after 3pm hurt my sleep. See [[notes/caffeine.md]].
```

`notes/caffeine.md` does not exist yet, so the link is recorded as `dangling`. Create that note, and the link resolves by itself:

```
dreams note put notes/caffeine.md <<'EOF'
---
title: Caffeine
---
Coffee after noon is a bad idea.
EOF
dreams note get notes/sleep.md      # dangling is gone; link: [did:key:…]
```

Then give your agents the vault:

```
claude mcp add-json -s user dreams "$(dreams mcp-json)"
```

See [Agents over MCP](#agents-over-mcp) for one setting that Claude Code needs.

## How it works

A few ideas explain most of dreams.

**Everything is a fact.** The database stores only facts: a relation (`the`), an entity (`of`), and a value (`is`). For example, `dreams.note/title` of `did:key:z6Mk…` is `"Sleep"`. There are no tables and no documents in storage. A "note" is a view of the facts that share one entity.

**A note is an entity with a path.** Each note gets a random id (a `did:key`) when it is created. Its path, such as `notes/sleep.md`, is only another fact. So you can rename a note, and every link to it still works, because links point to the entity and not to the path.

**Relations have a domain and a type.** `dreams.note/title` is the relation `title` in the domain `dreams.note`. Each relation also has a value type (`text:`, `natural:`, `entity:`, and so on) and a *pick*, which says how to read it when it has more than one value:

| Pick | Means | Example |
|---|---|---|
| `last:` | One value. A new value replaces the old one. | `title`, `content`, `path` |
| `all:` | A set. Each value is its own fact. | `tag`, `link` |
| `min:` / `max:` | Keep the lowest or highest. | `created`, `updated` |

**History is kept.** Every write is a new commit. Old values are retracted, not erased, and `dreams note history` shows them.

**Writes always land.** There are no revision numbers to send back, and no "stale write" errors. If two devices change the same value without seeing each other, a sync keeps both values, and dreams shows that as a [conflict](#conflicts) for you or an agent to settle.

**Kinds are structural.** Nothing records "this entity is a task". An entity is a task because it has the task relations (`dreams.task/runner`, `every`, `prompt`). Such a named set of relations is a *concept*. You can define your own; see [Your own kinds of things](#your-own-kinds-of-things).

**Two repositories.** `vault` holds everything that syncs. `local` holds what belongs to one device: which tasks run here, their schedules and leases, and the remotes. `local` never syncs.

## Notes

### Writing

`dreams note put PATH [FILE]` writes the note at `PATH` from a file, or from stdin. The extension of `PATH` picks the format: `.md` is Markdown with YAML frontmatter, `.json` is JSON, and `.yaml` or `.yml` is YAML.

In Markdown, the text after the frontmatter is `content`. In the frontmatter:

- `title` and `tags` become `dreams.note/title` and one `dreams.note/tag` per tag.
- A key that names a domain, with a map below it, writes relations in that domain:

  ```yaml
  ---
  title: Caffeine
  me.health:
    half-life-hours: 5
  ---
  ```

  This writes `me.health/half-life-hours = 5`.
- Any other key goes in the domain `dreams.json`.

`note put` on a path that exists updates that note. Send only what you want to change: relations you leave out keep their values. The output of `note get` is valid input, so this is a full edit cycle:

```
dreams note get notes/sleep.md > sleep.md
$EDITOR sleep.md
dreams note put notes/sleep.md sleep.md
```

To rename a note, put its file at the new path. The `entity` line in the frontmatter says which note it is:

```
dreams note get notes/caffeine.md > c.md
dreams note put notes/coffee.md c.md
```

dreams stamps `created`, `updated`, and `by` (who wrote it) on every entity that a write touches. You never set them yourself.

### Links

When `content` changes, dreams reads the links in it:

| In content | Becomes |
|---|---|
| `[[notes/x.md]]`, `[[notes/x.md\|label]]` | A `dreams.note/link` to that note |
| `[[Some Title]]` (no note has that path) | A link to the note with that title |
| `dreams:///notes/x.md`, or a bare `did:key:…` | A link to that note |
| A target that matches nothing | A `dreams.note/dangling` value, resolved when the note appears |
| `[[dreams://alice/notes/x.md]]` | A `dreams.note/xlink`: a link into a vault you follow |

Backlinks cost nothing extra: the database already indexes facts by value. The seeded rule `dreams.note/backlink` lets queries read them as a relation, and the MCP tools `backlinks` and `neighbors` walk the graph.

### Reading, searching, deleting

```
dreams note get notes/sleep.md           # also: dreams:///notes/sleep.md, a did:key, or dreams://alice/…
dreams note list --tag health --prefix notes/
dreams note search "afternoon naps" --tag health
dreams note history notes/sleep.md --attribute dreams.note/content
dreams note rm notes/sleep.md
dreams changes --since 1792316000000     # notes updated at or after a time, in Unix ms
```

Search is full text, with BM25 ranking: a match in the title counts 10 times, in tags 5 times, and in content once. Snippets mark the matched words with `**`. An empty search lists notes newest first.

`note rm` retracts every fact of the note. If another device edited the note at the same time, its edit survives the sync, and you can be left with a *husk*: for example, a title with no path. `dreams doctor` lists husks, and also two notes that claim the same path after a sync.

## Your own kinds of things

You do not need a schema to store a new kind of thing. Write the facts:

```
dreams note put recipes/soup.yaml <<'EOF'
title: Soup
me.recipes:
  servings: 2
EOF
```

When the shape settles, name it. A *schema note* is a note under `schemas/`, with the tag `schema`, whose content is a concept in Dialog's notation:

```yaml
description: A recipe
with:
  servings:
    the: me.recipes/servings
    as: "natural:"
  source:
    the: me.recipes/source
    as: "text:"
    optional: true
```

An agent writes one with the `define` tool. From then on, queries can use the concept by name (`{"concept": "schemas/recipe.yaml"}`), and `put_note` with `concept` refuses notes that lack a required field. Entities that do not fit simply do not match. Nothing is migrated or deleted.

`dreams describe` is the map of the vault. It lists every relation in use with its type, pick, and count; every concept; and every installed rule. A relation that no concept declares is marked `undeclared`, with the types seen and `multi: true` when some entity holds more than one value. Agents call it before they write, so that the same kind of thing does not get two names.

By convention, use the domain the person names. If there is none, use `me.<kind>`, such as `me.recipes`. Do not write in `dialog.*` (Dialog's own) or `dreams.*` (dreams'), except through the dreams tools.

## Asking questions: queries

`dreams query FILE` (or `-` for stdin) runs a Datalog rule in Dialog's *formal notation*, as JSON or YAML. It runs once and installs nothing. The result is one JSON object per row.

Notes with a dangling link, and their paths:

```yaml
deduce: {find: [note, path, target]}
when:
  - assert: {with: {is: {the: dreams.note/dangling, as: "text:", pick: "all:"}}}
    where: {this: {"?": {name: note}}, is: {"?": {name: target}}}
  - assert: {with: {is: {the: dreams.note/path, as: "text:"}}}
    where: {this: {"?": {name: note}}, is: {"?": {name: path}}}
```

```
dreams query dangling.yaml
[{"note": "did:key:z6Mk…", "path": "notes/sleep.md", "target": "notes/caffeine.md"}]
```

Each premise under `when` matches facts of one relation. `{"?": {"name": "note"}}` is a variable. A variable used in two premises joins them. A plain value, such as `is: health`, filters.

dreams adds two shorthands, and passes everything else to Dialog as it is:

- `deduce: {find: [a, b]}` names the result columns.
- `{assert: {concept: task}, where: {…}}` matches a whole concept: a built-in name (`note`, `task`, `runner`, `run`, `feed`, `feed-item`, `skill`, `prompt`, `bookmark`) or a schema note path.

Queries can also use `unless` (negation), comparisons (`==`, `<`, `>=`, `text/like`), and `reduce` (`count`, `count-distinct`, and so on). They time out after 5 seconds, and return 100 rows unless you pass `--limit` (up to 1000).

**The one trap: picks.** A premise reads a relation with the pick it states, and `last:` is the default. A premise on `dreams.note/tag` without `pick: "all:"` sees one tag per note, and misses matches without an error. Copy the descriptors from `dreams describe`; they have the right pick.

The seeded `dialog` skill (`skill://dialog/SKILL.md`) teaches agents the notation, with a worked example for each common question: notes with a tag, backlinks, notes by one writer, notes changed since a time, orphan notes, tag counts, conflicts, a task's runs, and dangling links.

### Rules

A query that is useful often can be installed as a *deductive rule*. It then works like a relation that is computed on read:

```
dreams rule add rule.yaml
dreams rule list
dreams rule rm rule:4WqZ…
```

Rules are stored as facts, so they sync. Two are seeded: `dreams.note/backlink`, and `dreams.daily/date` (notes whose path is a date, like `2026-10-18.md`). An installed rule cannot use `reduce`. If a sync creates a cycle through negation, Dialog sets the rule aside, and `dreams doctor` lists it.

### Why the formal notation?

It is verbose, but it is what Dialog stores as rules, and it has a published JSON schema. An agent that can query can also define rules, with no translation between the two. When Dialog ships an expander for its short notation (`?var`), dreams will accept that too.

## Agents over MCP

`dreams serve` is an MCP server over stdio. It speaks the stateless protocol, version 2026-07-28, only. `mcp-json` prints the entry that a host needs, with absolute paths:

```
claude mcp add-json -s user dreams "$(dreams --vault ~/.dreams mcp-json)"
```

Claude Code needs protocol negotiation turned on to connect to a 2026-07-28 server. Add this to `~/.claude/settings.json`, or export the variable in your shell profile:

```json
{ "env": { "MCP_PROTOCOL_NEGOTIATION": "auto" } }
```

Run `/mcp` in Claude Code to check that `dreams` is connected. Other hosts take the same entry under `mcpServers`.

### Tools

| Tool | Does |
|---|---|
| `describe` | The map: relations in use, concepts, rules. Start here. |
| `query` | A Datalog rule in formal notation, run once, read-only. |
| `search` | Full-text search, with `tag`, `prefix`, `limit`. |
| `get_note` | A note by path, `dreams://` URI, or entity, as Markdown with frontmatter. |
| `put_note` | Create or update by `path` or `entity`: `title`, `content`, `tags`, `fields` (`{relation: value}`). Optional `concept` validates; optional `expect_updated` refuses the write if the note changed since you read it. |
| `delete_note` | Retract every fact of a note. |
| `transact` | Assert and retract raw facts in one commit. |
| `define` | Write a concept as a schema note, or install a rule. |
| `backlinks`, `neighbors` | The link graph around a note. |
| `history` | Every value a note's relations have held. |
| `changes` | Notes updated since a time. |
| `list_conflicts`, `resolve_conflict` | See [Conflicts](#conflicts). |
| `deploy_task`, `disable_task`, `list_tasks`, `run_task` | See [Dreaming](#dreaming-scheduled-tasks). |
| `pull_feeds` | See [Feeds](#feeds). |

`query`, `describe`, and `search` take an optional `vault`: the petname of a vault you follow.

Every note is also a resource at `dreams:///<path>`. Notes with `dreams.skill/name` are served as skills (`skill://<name>/SKILL.md`, through the MCP Skills extension), and notes with `dreams.prompt/name` as prompts (in Claude Code, `/dreams:<name>`). A host that opens `subscriptions/listen` is told when these change.

### What agents may not do

- Write in `dialog.*` or `dreams.deploy/*` with `transact`.
- Change *protected* notes: the seeded runners and run receipts. The CLI can.
- Change a version, or write `dreams.version/*`.
- Deploy a task. Only you can, after you see what will run.

### Seeded skills and prompts

| Skill | Teaches |
|---|---|
| `dreams` | How the vault works, and how to set up tasks, runners, feeds, skills, and prompts |
| `dialog` | Queries, facts, concepts, and rules |
| `daily-note` | A note per day at `YYYY-MM-DD.md`, with an intention |
| `bookmark` | Save a web page as a note, with a summary |
| `brief` | A daily page of food for thought, made from your own notes |

The prompts `/dreams:daily`, `/dreams:intention`, `/dreams:bookmark`, and `/dreams:brief` use them. Skills and prompts are ordinary notes: edit them, or write your own. `dreams restore-defaults` puts the seeded ones back.

## Dreaming: scheduled tasks

A task wakes an agent with a prompt: every interval, or when the rows of a query change. Tasks, runners, and run receipts are notes, so they sync. But a task runs only on the devices where you *deploy* it.

### A first task

```
dreams runner list                     # claude, codex, pi: pick one whose CLI is installed and logged in
dreams task add tasks/digest.md --runner runners/claude.json --every 1d \
  --prompt 'Read the notes tagged inbox and write a short digest note tagged digest.'
dreams task deploy tasks/digest.md     # shows the prompt and the exact command; asks y/N
dreams task run tasks/digest.md --now  # fire once, here, and print the receipt
dreams task runs tasks/digest.md       # past runs, newest first
dreams daemon install                  # from now on, run it at login
```

The agent runs in the folder `workspace` in the vault (or the task's `--cwd`). It has `dreams` on its `PATH`, `DREAMS_VAULT` pointing at this vault, and an MCP config for it.

### Triggers are queries

A task with `when` fires only when the rows of its query differ from the rows at its last run. `every` is then the shortest gap between runs.

```
dreams task add tasks/triage.md --runner runners/claude.json --every 15m --tag inbox \
  --prompt 'Triage the notes listed below.'
```

`--tag`, `--glob 'notes/*'`, and `--type <concept>` write a `when` query for you. `--when FILE` takes any query, so any question can be a trigger:

- a note gained a dangling link (someone wished a note existed);
- an item arrived from a feed tagged `research`;
- a daily note has an intention but no brief.

The rows that changed are added to the end of the prompt, under "Changed since last run:". Each run writes as its own session, and rows whose note was last written by the task itself are left out. So a task does not wake itself up.

`task check` shows whether a task is due and which rows changed. Nothing runs.

### Deploys

A deploy freezes the task and its runner as a *version*: a copy of the prompt, schedule, query, folder, command and timeout, whose id is the hash of its facts. The scheduler runs that version, never the live note. So an edit to the task or its runner, made here, by an agent, or through a sync, does not change what runs. `task list` shows the task as `deployed (changed)`, the deployed version keeps running, and the edit runs after you deploy again. This matters because a runner is a command line: nothing runs before you have seen it.

Before each run, the scheduler checks that the version's facts still match its id, and refuses to run one that was altered. Deleting the task note or its runner note stops the task.

Over MCP, `deploy_task` asks you through your MCP client (elicitation) to confirm the prompt, command, folder, query, and timeout. The agent cannot answer for you. `run_task` asks the scheduler to fire a deployed task on its next tick.

### Runners

A runner is an argv, run without a shell. The prompt goes to stdin, and the last message is read from stdout (or from the `{out}` file). The tokens `{vault}`, `{task}`, `{run}`, `{mcp}`, `{out}`, and `{exe}` are replaced. The environment has `DREAMS_VAULT`, `DREAMS_TASK`, `DREAMS_RUN`, `DREAMS_SESSION`, and `DREAMS_MCP`.

```
dreams runner add runners/claude-fast.json --timeout 5m -- \
  claude -p --model claude-sonnet-5-5 --mcp-config {mcp} --strict-mcp-config
```

The seeded `runners/claude.json` runs Claude Code headless, with its Bash sandbox on and the dreams MCP tools allowed. `codex` and `pi` are the other seeded agents. `runners/feeds.json` is not an agent: it runs `dreams feed pull`.

### Receipts, leases, the daemon

Each run writes a receipt note at `runs/<task>/<uuid>.md`: the task, the version that ran (`dreams.run/version`), start and end times, exit code, error, and the agent's last message as content. Receipts sync, so `task runs` shows runs from every device, and `dreams note get <version>` on any device shows exactly what ran.

Before the agent starts, the scheduler takes a lease in `local` for the runner's timeout plus a minute. Two schedulers on one vault therefore do not run a task twice. If dreams crashes mid-run, the next tick finishes the bookkeeping without running the agent again.

`dreams daemon` ticks every `--interval` (1m), and sooner when the vault changes. When the vault has remotes, it also syncs after each tick. `daemon install` registers it with launchd (macOS) or systemd (Linux), with your current `PATH`. Logs go to `~/Library/Logs/dreams/` on macOS, and to `journalctl --user -u io.dreams.vault-<id>` on Linux. `dreams tick` runs one pass by hand.

### Seeded tasks

Both are dormant until you deploy them:

- `tasks/brief.json` makes a daily brief and adds it to today's daily note. `every: 1d` counts from the deploy, so deploy it at the time of day you want it. Deploy it on one device only.
- `tasks/pull-feeds.json` pulls every feed each hour.

## Feeds

```
dreams feed add https://news.ycombinator.com/rss
dreams feed add https://example.com/changelog --kind html --instructions "Note only breaking changes."
dreams feed pull
```

A feed is a note with a URL, a kind (`rss` or `html`), and optional `instructions` for the agent that reads its items. A pull writes each new item as a note and prints only the new ones. An `html` feed is one page, and a change to the page's text is a new item.

An item's id is derived from the feed and the item's key (its guid, else its link, else its title and date). So two devices that pull the same feed write the same entities, and a sync merges them instead of duplicating them. Deleting an item keeps its feed and key, so a later pull does not bring it back.

To wake an agent on new items, deploy `tasks/pull-feeds.json`, then add a task that triggers on items: `--type feed-item`, or a `--when` query on one feed.

Item text comes from outside the vault, and the agents that read it have tools. Treat it as data, not instructions. The MCP server's instructions and the `dreams` skill tell agents so.

## Sync between your devices

A *remote* is a place that holds a copy of the vault. dreams pulls from it and pushes to it. It can be:

- a folder: a NAS, a USB disk, or a Syncthing or Dropbox folder;
- `s3://<bucket>?endpoint=<url>&region=<r>&path-style=true`, for S3 or R2, with `AWS_ACCESS_KEY_ID` and `AWS_SECRET_ACCESS_KEY` set when you add it;
- `ucan+https://<access service>`, where a UCAN delegation is the only credential.

On the first device:

```
dreams remote add origin /Volumes/nas/dreams      # an empty folder becomes this vault's remote
dreams sync                                       # pull, then push
```

To add a second device, it needs a delegation from the first:

```
laptop-2$ dreams device id                        # prints laptop-2's did:key
laptop-1$ dreams device add did:key:z6Mk…         # writes the delegation into each remote folder
laptop-2$ dreams remote add origin /Volumes/nas/dreams
laptop-2$ dreams sync
```

A bucket or a UCAN service has no place for the delegation, so for those remotes `device add` writes it in the vault folder instead, under `delegations/`, and prints the path. Copy the file to the new device, and pass `--repository <vault DID> --delegation <file>` to `remote add` there.

- **Sync is a merge, never an overwrite.** A pull merges the remote's facts into yours, per relation. A push is a compare-and-swap; if another device pushed first, dreams pulls and tries again, up to three times.
- **The daemon syncs** after each tick.
- **A remote folder is safe in Dropbox.** A remote is blocks plus a compare-and-swap head, made for "dumb" shared storage. The live vault (`~/.dreams`) is not: keep it on a local disk.
- **Remotes are not encrypted yet.** Dialog stores data on remotes in the clear; encryption is planned upstream. Use storage you trust.
- **Revoking is only as strong as the remote.** `dreams device revoke <did>` posts a revocation to a UCAN service, which then refuses the device. A folder or a bucket checks nothing: anyone who can read it keeps reading it. There, revoking removes the delegation file, and you should move to a new remote.

## Conflicts

Most concurrent edits merge by themselves, because a sync merges each relation of each entity on its own. If one device changes a note's title and another changes its tags, both changes stay.

A *conflict* is one `last:` relation that holds two values after a sync, because two devices set it without seeing each other. Every device reads the same winner, so nothing breaks, but the other value is still there:

```
dreams note conflicts                 # notes whose path, title, or content hold several values
dreams note diff notes/plan.md        # each value as a diff from the value before both edits
```

To settle one:

```
dreams note resolve notes/plan.md               # take the line-by-line merge, when no line changed on both sides
dreams note resolve notes/plan.md merged.md     # take your own merge
dreams note resolve notes/plan.md --auto        # let a runner merge what code cannot
```

`content` merges three ways, line by line, as git does. Where both sides changed the same lines, the merge has conflict markers, and `--auto` sends only those blocks to the runner (`runners/claude.json` unless you pass `--runner`). The runner replies with edits to the marked blocks, so it cannot rewrite the rest of the note. A merge that still has markers is refused. Resolving retracts every value and asserts the merged one, and the old values stay in history.

Over MCP, an agent uses `list_conflicts` and `resolve_conflict`. `--attribute` resolves a relation other than `content`.

## Publishing and following

Your vault is private. To share notes, publish them to a second repository, `public`, with its own remote:

```
dreams remote add origin /shared/alice-public --public
dreams publish notes/hello.md               # copy the note into public, then push
dreams peer grant did:key:z6Mk…             # let a reader's device read it
```

A reader follows it under a *petname*, a name of their choosing:

```
dreams peer add alice /shared/alice-public
dreams note get dreams://alice/notes/hello.md
dreams note search --peer alice hello
```

The followed vault is a read-only replica, which the daemon keeps fresh, with its own search index. In your notes, `[[dreams://alice/notes/hello.md]]` becomes a link into it.

- A published note is a copy of its facts at the time. `publish` again to update it.
- A query runs against one vault. To relate Alice's notes to yours, query both, and write notes that link with `dreams://` URIs.
- `peer grant` grants the whole `public` repository. The same limits on encryption and revocation apply as for [sync](#sync-between-your-devices).

## Import and export

```
dreams import ~/old-notes                   # every .md, .json, .yaml, .yml under the folder
dreams import api-dump.json --domain me.github
dreams export ~/backup                      # every note, written back to its path
```

A file's path relative to the folder is its note's path. Importing again updates the same notes. In JSON and YAML:

- `title`, `content`, and `tags` are the note's fields.
- A key that names a domain, such as `dreams.task` or `me.recipes`, holds that domain's relations. This is the shape `export` and `note get` write.
- A string `dreams:///<path>` is a link to the note at that path.
- Other keys go in the `--domain` (default `dreams.json`), as `<domain>/<key in kebab-case>`. A key that is not a valid relation name is reported and skipped, not renamed.
- An array of values is a set: one fact per value. **Order and duplicates are lost.**
- A nested object, or a list of objects, becomes a child entity linked from the parent, with an id derived from the parent and the key, so a re-import updates it. Lists of objects keep their order.
- `null` writes nothing.

Export writes the facts back as nested JSON, YAML, or Markdown. It is faithful to the facts, not to the original bytes.

## Where things live

```
~/.dreams/
  dialog/      the database: the vault, local, and public repositories, and keys
  index/       the search index; derived, never synced, safe to delete
  workspace/   where task agents run, by default
```

`dreams reindex` rebuilds the search index. Several processes can use the vault at once (the CLI, MCP servers, the daemon): each commit is a compare-and-swap on the branch head, and dreams retries on a clash. That lock is not safe on network filesystems, so keep `~/.dreams` on a local disk and share through a remote.

## FAQ

**Can I edit my notes in Obsidian or a text editor?**
Not in place: the vault is a database, not a folder of files. `dreams export` a folder, edit it, and `dreams import` it back; or use `note get` and `note put`. Links in `[[wiki]]` form work the same in both.

**Why Datalog rather than SQL or plain files?**
Because the questions that matter in a wiki are about relations: what links here, what is missing, what changed, what an agent wrote. In Datalog those are short queries, and the same rule can be a trigger for a task. A fact model also makes sync a merge per relation, not per file.

**Do I have to learn the query language?**
No. `note`, `search`, and the MCP tools cover everyday use, and agents read the `dialog` skill. Queries are for questions the tools do not cover.

**Do I need a schema?**
No. Write any relation. Define a concept when you want names in queries, or validation on write.

**How do I see what an agent did?**
Every write is stamped with the writer's session DID in `dreams.note/by`, and each task writes as its own session. Query by `by`, read `note history`, or read a task's receipts with `task runs`.

**Can an agent run code on my machine without asking?**
Not through dreams. A task runs only after you deploy it, and the deploy shows the exact command. The scheduler runs the version you confirmed. An edit to the task or its runner, including one from a sync, runs only after you deploy it.

**What happens when I delete a note on one device and edit it on another?**
The edit survives the sync. You may be left with a husk, a note with some fields but no path. `dreams doctor` lists husks.

**Is my data encrypted?**
Not on remotes, yet. See [Sync](#sync-between-your-devices).

## Command reference

```
dreams [--vault DIR] [--json] [--session NAME] <command>

  init | restore-defaults                     create the vault; seed the built-ins (again)
  serve | mcp-json                            MCP over stdio; print the host's server entry

  note put <path> [FILE]                      create or update (Markdown, JSON, or YAML by extension)
  note get <path | dreams://… | did:key:…>
  note list [--tag T] [--prefix P] [--limit N]
  note search <q> [--tag T] [--prefix P] [--peer NAME] [--limit N]
  note history <note> [--attribute A]
  note rm <note>
  note conflicts | diff <note> | resolve <note> [FILE] [--auto] [--runner R] [--attribute A]

  query <FILE|-> [--limit N]                  Datalog in formal notation, JSON or YAML
  describe [domain]                           relations, concepts, and rules in use
  rule add <FILE|-> | list | rm <entity>
  changes [--since MS] [--limit N]
  doctor                                      husks, path collisions, set-aside rules
  reindex
  import <dir|file> [--domain D] | export <dir>

  task add <path> --runner R --every 1h --prompt TEXT|- [--when FILE] [--tag T] [--glob G] [--type C] [--cwd DIR]
  task list | deploy [<task>] [--yes] | disable <task> | rm <task>
  task check <task> | run <task> [--now] | runs <task>
  runner add <path> [--timeout 10m] -- <command>... | list
  feed add <url> [--kind rss|html] [--path P] [--instructions TEXT] | list | pull [<feed>]
  tick | daemon [--interval 1m] [--poll 2s] | daemon install | uninstall

  remote add <name> <location> [--public] [--repository DID] [--delegation FILE] | list | rm <name>
  pull | sync
  device id | add <did> | revoke <did>
  peer add <name> <location> | list | rm <name> | grant <did> | revoke <did>
  publish <note>
```

`--json` prints the same structures the MCP tools return. Errors print as a JSON object on stderr, with exit code 1.

## Development

```
cargo test
cargo clippy --all-targets
cargo fmt --check
```

`SPEC.md` is the design. `tests/dialog_smoke.rs` pins every Dialog behavior dreams relies on; run it first after upgrading Dialog. `tests/sync.rs` syncs two vaults through folder, S3, and UCAN remotes; the S3 and UCAN tests start local test servers. CLI tests run commands in process against temporary vaults, and drive the scheduler with runners such as `cat`.
