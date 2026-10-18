# dreams — specification

Status: draft, 2026-10-16. Successor to the `dreams-cli` spike (`../dreams-cli`).

dreams is a memory substrate for agents. It is a self-contained Rust binary that holds a person's notes and their agents' notes as facts in a [DialogDB](https://github.com/dialog-db/dialog-db) repository. It serves them over MCP, wakes background ("dreaming") agents on a schedule or when data changes, and syncs with the person's other devices and with other people's public vaults.

This spec re-imagines the spike on Dialog. The spike's lessons carry over. Its storage, revision and sync machinery does not, because Dialog already does those things.

---

## 1. Goals

- **A subconscious for your agents.** A shared, durable LLM wiki that every agent can read and write over MCP. Agents leave notes for each other and for the person.
- **Dreaming agents.** Background agents wake on a schedule or when a query's results change. They explore the wiki and write back insights, briefs and links.
- **A tool for thought.** The person reads and writes the same notes as plain Markdown.
- **Agents as automations.** Feeds, digests and periodic chores are tasks too.
- **Self-contained and user-owned.** A single Rust binary with no server. Data lives on the person's disk and in remotes they choose.
- **Decentralized and multiplayer.** Sync between devices, and follow other people's public vaults. Link to their notes with `dreams://petname/path`.

### Non-goals (for now)

- A GUI. The filesystem, the CLI and MCP clients are the UI.
- Collaborative real-time text editing. A note's content is one value, so concurrent edits produce two values to merge (§6), not a CRDT.
- Embeddings and semantic search. Left for later; the search sidecar (§9) is where they'd go.
- WASM or a browser vault. Dialog supports it; dreams does not need it yet.
- Cross-vault joins in one query (§12).

### Status of the substrate

Dialog is experimental: 0.4.0-rc.2 as of 2026-10-15, with breaking changes most days and no migration promise. dreams pins Dialog to a git revision and upgrades on purpose. Every Dialog API this spec relies on is listed in §15 so that an upgrade has a checklist.

---

## 2. Concepts in one page

| Term | Meaning in dreams |
|---|---|
| **Vault** | A Dialog **repository** named `vault`. Everything that syncs lives here. |
| **Local repo** | A second repository named `local`. It is never given a remote and holds per-device state: deploys, leases, cursors. |
| **Fact** | `{the, of, is}`: a relation (`the`), an entity (`of`) and a value (`is`). The only thing stored. |
| **Entity** | A URI. Notes get a random `did:key`. Feed items and similar get content-derived ids. |
| **Attribute** | A relation plus a type (`as`) plus a pick. Example: `dreams.note/tag`, `text:`, `all:`. |
| **Concept** | A named set of attributes. An entity *is* a Task if it has the task attributes. |
| **Rule** | A deductive rule in Dialog notation. Stored as facts, so it syncs. |
| **Note** | Any entity with a `dreams.note/path`. The ergonomic, Markdown-shaped view of an entity. |
| **Remote** | Where a repository pushes and pulls: a folder, S3/R2, or a UCAN access service. |
| **Petname** | The person's local name for someone else's vault: `alice` → (repository DID, remote). |
| **Session** | The `did:key` a process or agent run writes as, delegated by UCAN from the device. |

The vault folder (default `~/.dreams`; set with `--vault` or `DREAMS_VAULT`) looks like this:

```
~/.dreams/
  dialog/      Dialog filesystem storage: the vault and local repositories, keys
  index/       Tantivy search index (derived, never synced, safe to delete)
  workspace/   default cwd for task runs
```

---

## 3. Data model

### 3.1 Domains

All dreams relations live under `dreams.*`. `dialog.*` is reserved by Dialog. Users and agents may invent any other domain.

The convention for invented domains, taught by the `dialog` skill but not enforced:
- Use the domain the user names. If the user names none, use `me.<kind>`, for example `me.recipes`.
- Call `describe` before inventing a domain, so the same kind of thing doesn't get two names.

| Domain | Holds |
|---|---|
| `dreams.note` | Fields shared by every note: path, title, content, tags, links, timestamps |
| `dreams.task`, `dreams.runner`, `dreams.run` | Scheduled agents (§10) |
| `dreams.feed`, `dreams.feed-item` | Feeds (§11) |
| `dreams.skill`, `dreams.prompt` | Things served over MCP (§11) |
| `dreams.peer` | Petnames (§12) |
| `dreams.deploy` | Deploy state, **local repo only** |
| `dreams.json` | The default domain for imported keys with no better home (§7) |
| `dreams.query` | Scratch output columns for one-off queries (§8) |

### 3.2 The note

A note is an entity. Its identity is a random `did:key` (`Entity::new()`), not its path.

| Attribute | `as` | pick | Notes |
|---|---|---|---|
| `dreams.note/path` | `text:` | `last:` | e.g. `notes/sleep.md`. A name, not an identity; renaming is one assertion. |
| `dreams.note/title` | `text:` | `last:` | |
| `dreams.note/content` | `text:` | `last:` | Markdown. Values over 4 KiB spill to Dialog's asset store automatically. |
| `dreams.note/tag` | `text:` | `all:` | One fact per tag. |
| `dreams.note/link` | `entity:` | `all:` | Outgoing links, derived from content on write (§3.3). |
| `dreams.note/dangling` | `text:` | `all:` | `[[targets]]` that resolve to no note yet. |
| `dreams.note/created` | `natural:` | `min:` | Unix ms. `min:` keeps the first value even when two devices disagree. |
| `dreams.note/updated` | `natural:` | `max:` | Unix ms of the latest write through dreams. |
| `dreams.note/by` | `text:` | `last:` | The session DID of the latest writer (§10.4). |

Every entity dreams creates (task, runner, skill, feed item, run receipt) is also a note. It has a path and may have a title, content and tags. This keeps the spike's "blessed fields" idea: one search index, one Markdown view and one set of tools cover everything.

Dialog has no wall-clock time; editions are causal depth. So `created`, `updated` and `by` are plain facts that dreams writes. Every write through dreams, `transact` included, stamps `updated` and `by` on each entity it touches. Search catch-up (§9) and `changes` depend on that. That makes them queryable, which the spike's `_actor` and `_created_at` columns were not without SQL.

### 3.3 Links

On every write of `content`, dreams parses links out of it:

- `[[path]]`, `[[path|label]]`, and `[[title]]` matched against titles when no path matches.
- `dreams:///path` (this vault) and bare `did:key:…` entity URIs.

It then replaces the note's `dreams.note/link` set with the entities they resolve to, and its `dreams.note/dangling` set with the targets that resolve to nothing. When a note is created at a path that some `dangling` value names, dreams resolves those links in the same transaction.

Backlinks need no extra storage. A query for `dreams.note/link` with the value bound reads Dialog's VAE index. Links into other vaults (`dreams://alice/…`) are kept as text in `dreams.note/xlink` (`text:`, `all:`), because an entity URI alone doesn't say which repository holds it.

### 3.4 What goes away from the spike

| Spike | Here | Why |
|---|---|---|
| `_id` (path is identity) | `dreams.note/path` + random entity | Renames don't break links. Answers the open question in the old `docs/SPEC.md` (`_path` vs `_id`). |
| `_rev`, `_parent`, generations, `Conflict` on a stale parent | Gone | A write asserts facts. Dialog keeps history, and `last:` supersedes. Agents no longer have to fetch a rev before writing. |
| Tombstones, `docs_winners` view | Gone | Retraction plus Dialog's watermark; deletes survive merges without tombstones. |
| `_type` pinned to `?rev=` | Structural typing (§5) | Concept identity is already a content hash. |
| `_conflicts` | A query (§6) | Concurrent values stay visible under `all:`. |
| `doc_heads`, `doc_tags`, `docs_fts` mirrors and triggers | Dialog indexes + Tantivy (§9) | |

**Writes are no longer conditional.** The spike refused an update whose `_parent` was stale; here a write always lands. Lost-update protection for long content comes from the conflict view (§6), not from refusing writes. A `put_note` may still pass `expect_updated` (the `updated` value it read). dreams then refuses the write if the current value differs, for agents that want compare-and-set.

### 3.5 Deleting

`dreams note rm` retracts every fact of the entity. Dialog's watermark makes the removal survive a merge.

A concurrent edit on another device survives, because a retraction removes only the exact versions it saw. The result can be a **husk**: an entity with a `title` but no `path`, for example. Husks are listed by a query (§6) and shown by `dreams doctor`. This is the Dialog form of the spike's "an edit beats a delete".

---

## 4. Worked example: one spike document as facts

The spike's seeded `tasks/brief.json`:

```json
{
  "_type": "doc://schemas/task.json",
  "title": "Daily brief",
  "runner": "doc://runners/claude.json",
  "every": "1d",
  "prompt": "Use the brief skill and the daily-note skill. …"
}
```

becomes these facts. `E` is a fresh `did:key`, and `R` is the entity whose path is `runners/claude.json`.

| the | of | is |
|---|---|---|
| `dreams.note/path` | E | `"tasks/brief.json"` |
| `dreams.note/title` | E | `"Daily brief"` |
| `dreams.task/runner` | E | R (`entity:`) |
| `dreams.task/every` | E | `"1d"` |
| `dreams.task/prompt` | E | `"Use the brief skill and the daily-note skill. …"` |
| `dreams.note/created` | E | `1760000000000` |
| `dreams.note/updated` | E | `1760000000000` |
| `dreams.note/by` | E | `"did:key:z6Mk…(importer session)"` |

`_type` produced no fact. The entity is a Task because it has the Task concept's required attributes (§5).

---

## 5. Schemas become concepts

### 5.1 Built-in concepts

dreams defines its built-in concepts in Rust with `#[derive(Attribute)]` and `#[derive(Concept)]` (`dialog-macros`). They are also exported as notation by `dreams describe`. The Task concept in formal notation (YAML) looks like this:

```yaml
description: A scheduled agent. Fires every interval, or when its `when` query's results change.
with:
  runner:
    description: The runner that executes this task
    the: dreams.task/runner
    as: "entity:"
  every:
    description: Interval, e.g. 30m, 1d
    the: dreams.task/every
    as: "text:"
  prompt:
    description: What the agent is asked to do
    the: dreams.task/prompt
    as: "text:"
  when:
    description: A query (Dialog notation, JSON) whose result changes wake the task
    the: dreams.task/when
    as: "text:"
    optional: true
  cwd:
    description: Working directory, relative to the vault folder
    the: dreams.task/cwd
    as: "text:"
    optional: true
```

The other built-ins, as attribute lists (all `last:` unless noted):

- **Runner:** `dreams.runner/command` (an ordered argv, §7.3), `dreams.runner/timeout` (`text:`)
- **Run:** `dreams.run/task` (`entity:`), `dreams.run/deploy` (`text:`, the deploy hash, §10.3), `dreams.run/peer` (`text:`, device DID), `dreams.run/started` and `dreams.run/finished` (`natural:`), `dreams.run/exit-code` (`integer:`, optional), `dreams.run/error` (`text:`, optional). The agent's last message goes in `dreams.note/content`.
- **Feed:** `dreams.feed/url`, `dreams.feed/kind` (`"rss"` \| `"html"`), `dreams.feed/instructions` (optional)
- **Feed item:** `dreams.feed-item/feed` (`entity:`), `dreams.feed-item/key`, `url`, `guid`, `published` (each optional except `feed` and `key`)
- **Skill / Prompt:** `dreams.skill/name` or `dreams.prompt/name`, `dreams.skill/description` or `dreams.prompt/description`. The body is `dreams.note/content`.
- **Daily note / Bookmark:** the spike's seeded schemas, ported the same way (`dreams.daily/intention`, `dreams.bookmark/url`).

### 5.2 User concepts

A user or agent defines a concept by writing a **schema note**. It has a path under `schemas/`, the tag `schema`, and content that is the concept in formal notation (JSON or YAML). This is the spike's "a schema is a document" convention, carried over unchanged. It still syncs, and it is still versioned by Dialog history.

`dreams describe` and the MCP `describe` tool (§8.2) read schema notes along with the built-ins.

### 5.3 Typing and validation

- **Typing is structural.** Nothing records "this entity is a Task". A query for the Task concept finds every entity that has its required attributes. Two vaults that define the same attributes have the same concept.
- **Validation is opt-in, on write.** `put_note` takes an optional `concept` (a schema note path, or a built-in name like `task`). dreams then refuses the write if a required field is missing or a value has the wrong type. The raw `transact` tool does not validate; Dialog "simply accretes".
- **No more pinning.** The spike pinned `_type` to a schema revision so old documents kept their old schema. Here, editing a schema note changes the concept's identity, and old entities simply match whichever concepts they still satisfy.

### 5.4 Rules

Agents and users may install **deductive** rules (`define` tool, `dreams rule add`). They are stored by Dialog under `dialog.rule/*` and sync with the vault. Useful seeded rules:

- `dreams.rule/backlink`: derives `dreams.note/backlink` from `dreams.note/link`, so concept queries can read backlinks as a field.
- `dreams.rule/tagged-daily`: daily notes by path pattern, as a concept.

**Inductive** rules, which run at commit time, are not exposed over MCP. An agent-installed rule that writes facts on every commit is too easy to get wrong silently. Rule cycles through negation, which can arise from merging, are quarantined by Dialog, and `dreams doctor` lists `dialog.rule/quarantined`.

### 5.5 Open-ended concepts

The vault has to hold kinds of things nobody planned for, such as recipes, people, or claims from papers, without a schema migration. Three pieces cover the path from an ad-hoc fact to a named concept:

1. **Invent.** An agent asserts facts in a new relation with `transact` or `put_note(fields)`. No definition is needed, because Dialog accepts any assertion.
2. **Discover.** `describe` lists every relation in use. A relation that no concept declares is reported as `undeclared`, with the value types seen and a `multi` flag that is true when some entity holds more than one value. `multi` is what tells the next agent to read the relation with `"pick": "all:"`.
3. **Name.** Once the shape settles, `define` writes it as a schema note (§5.2). Queries then use it by name (`{"assert": {"concept": "schemas/recipe.yaml"}, …}`, §8.3), and `put_note(concept=…)` validates new entities against it.

**Field names.** A built-in concept's field names are its attributes' names. For example, the Run concept's fields are `task`, `started`, `finished`, `exit-code` and so on. A schema note's field names are the keys of its `with`.

**Redefining.** Calling `define` again on the same path changes the concept. Entities that lack a newly required field stop matching it; nothing is deleted.

---

## 6. Conflicts

Dialog keeps concurrent writes. When two devices set `dreams.note/content` without seeing each other:

- A read under `last:` returns the winner of a deterministic election. Every replica picks the same one.
- A read under `all:` returns both values.

That gives the definition: **a conflict is an entity whose `last:` attribute has more than one value under `all:`**. Sequential writes never cause one, because `last:` retracts the value it supersedes.

`dreams note conflicts` and the `list_conflicts` query template (§8.3) find conflicts for `title`, `content` and `path`, or for any attribute given. This is the same aggregation every agent can write:

```json
{
  "deduce": { "with": {
    "note":  { "the": "dreams.query/note",  "as": "entity:" },
    "count": { "the": "dreams.query/count", "as": "natural:" } } },
  "when": [
    { "assert": { "with": { "is": { "the": "dreams.note/content", "as": "text:", "pick": "all:" } } },
      "where": { "this": { "?": { "name": "note" } }, "is": { "?": { "name": "content" } } } }
  ],
  "reduce": { "count": { "apply": "count-distinct", "of": { "?": { "name": "content" } } } }
}
```

The caller then keeps the rows with `count > 1`. Range constraints apply in `when`, before the fold, so the filter is done by dreams or by a second rule.

**Resolving.** The spike's merge design carries over unchanged, now in `resolve.rs` and `diff.rs`:

1. Fields that only one side changed need nothing; Dialog already merged them per attribute. This is a real simplification, since the spike merged fields by hand.
2. `content` is merged three-way, line by line, with git-style markers. The common ancestor is the value both conflicting versions superseded, read from Dialog history.
3. Whatever is still contested goes to a runner, which replies `{content_edits:[{old,new}]}`.
4. dreams retracts every value and asserts the merged one, and the conflict disappears. (A `last:` assertion alone retracts only the value it elects, so it would leave the other.)

**Path collisions** (two entities claiming the same `path` after a merge) and **husks** (§3.5) are queries too, shown by `dreams doctor`.

---

## 7. Importing JSON

There are two consumers: migrating a spike vault, and general "drop JSON in" imports such as exports from other tools or API dumps.

Dialog's `Importer` trait is only `Stream<Item = Result<Artifact, _>>`. The JSON importer is a function from a JSON value to a stream of artifacts, and that's the whole integration.

### 7.1 Input files

`dreams import <dir|file>` reads `.json`, `.yaml`/`.yml`, and `.md` (YAML frontmatter plus a body that becomes `content`).

- A file's relative path is its `dreams.note/path`, as in the spike.
- An existing note with that path is updated, not duplicated, so re-importing is idempotent.
- `--domain <d>` sets the domain for unmapped keys (default `dreams.json`).
- A `.jsonl` file imports one entity per line, with no path.

### 7.2 Mapping

| JSON | Facts |
|---|---|
| Object (the top-level document) | One entity: a random `did:key`, or the existing note at that path. |
| `_id` | `dreams.note/path` |
| `_type: doc://schemas/<x>.json` | No fact. The domain for the document's other keys becomes `dreams.<x>` for built-ins, or the domain the schema note's concept uses. |
| Other `_`-prefixed keys (`_rev`, `_parent`, …) | Dropped. |
| `title`, `content`, `tags` | `dreams.note/title`, `dreams.note/content`, one `dreams.note/tag` per tag. |
| Other key `k` | `<domain>/<kebab(k)>`. Keys that break Dialog's name rules, or push the selector past 64 bytes, are reported and skipped. They are never silently renamed. |
| String | `text:`. A string starting `doc://` becomes an `entity:` link to the note at that path, resolved in a second pass. Unresolvable ones stay text and are reported. |
| Number, boolean | The number or boolean value. |
| `null` | No fact. Absence is how Dialog says null. |
| Array of scalars | One fact per element, and the attribute is declared `all:`. Order and duplicates are lost, which is right for tags and sets. |
| Array that must keep its order | One `text:` value holding the array as JSON (§7.3), when the target concept marks the field ordered. |
| Nested object | A child entity linked by `entity:`. Its keys go under the domain `<domain>.<kebab(k)>`. Its id is content-derived from `(parent entity, key)` (`EntityExt::of`), so re-imports and edits update the same child. |
| Array of objects | Child entities with ids derived from `(parent, key, index)`, linked from the parent with `all:`. Each child carries `<domain>.<kebab(k)>/index` (`natural:`) so that export restores the order. |

The spike's task `when: {tag: "inbox"}` would become a child entity with `dreams.task.when/tag = "inbox"`. On migration, the importer instead rewrites the spike's `when` filters into a `when` query (§10.2), because that is how this design expresses them.

### 7.3 Ordered arrays

The only ordered field among the built-ins is `dreams.runner/command`, an argv. It is stored as one `text:` value holding a JSON array. Nothing queries an argv by element, so nothing is lost.

Dialog has a native answer to ordering: position names, a fractional index in the attribute name such as `dreams.runner.command/N5`. dreams does not use it yet. Position names fail `dialog_query::Relation` validation, so they can't be written through the same path as other facts. Revisit this when an imported list needs per-element queries.

### 7.4 Round-trip

`dreams export <dir>` writes each note back to `<path>`, using the format its extension implies, which is the spike's behavior. Unmapped domains come back as nested keys.

The original JSON bytes are not kept, so import followed by export is faithful to the facts, not to the bytes. That is deliberate: keeping a `record:` copy would be a second source of truth, and `record:` is not yet supported end to end in Dialog.

### 7.5 Migrating the spike

```
dreams-cli export /tmp/old && dreams import /tmp/old
```

Schemas under `schemas/` are converted to concept notation. `type` maps to `as`, `required` maps to the absence of `optional`, and `array` maps to `all:`. Anything JSON Schema says that a concept can't (patterns, enums, `minLength`) is reported. Those constraints move into dreams' write-time validation for built-ins, and are dropped for user schemas.

Run receipts and feed items are imported as ordinary entities. Conflicting revisions (`_conflicts`) are not migrated; resolve them in the spike first.

---

## 8. Querying, and the LLM's tools

### 8.1 Principles

- **The LLM speaks Dialog notation directly.** dreams does not invent a query language. The formal notation is a published JSON schema (`notes/notation/schema.json` upstream), it round-trips through serde (`DeductiveRuleDescriptor`), and it is what Dialog stores as rules. An agent that learns to query can also learn to define rules, with no translation in between.
- **`describe` comes first.** An agent can't write datalog against attributes it doesn't know. `describe` is the map, and its attribute descriptors are meant to be pasted into queries verbatim.
- **Canned tools for the common paths.** Reading a note, search and backlinks shouldn't require datalog.
- **Errors teach.** A query error says what was wrong and points to the `describe` entry or skill example that would fix it.

### 8.2 MCP tools

| Tool | Input | Output |
|---|---|---|
| `describe` | optional `domain` prefix | Attributes in use, each with its full descriptor (`the`, `as`, `pick`, `description`) and a fact count. Relations no concept declares are marked `undeclared`, with the value types seen and `multi` (some entity has more than one value). Also concepts (built-ins and schema notes) by name and in formal notation, and installed rules with their descriptions, quarantined rules flagged. |
| `query` | `rule` (formal notation, JSON), `limit` (default 100, max 1000) | Rows as JSON objects keyed by the head's field names. Read-only, with a 5 s timeout. |
| `search` | `q`, optional `tag`, `prefix`, `limit` | Tantivy hits: entity, path, title, snippet, score (§9). |
| `get_note` | `path`, `dreams://` URI, or entity | The note as Markdown with YAML frontmatter. Frontmatter holds the dreams fields and every other attribute of the entity, grouped by domain. |
| `put_note` | `path` or entity; `title`, `content`, `tags`, `fields` (`{the: value}`); optional `concept`, `expect_updated` | The entity. Updates `updated`, `by` and links. |
| `delete_note` | `path` or entity | Retracts every fact of the entity (§3.5). |
| `transact` | `assert`: `[{the, of, is}]`, `retract`: `[{the, of, is}]` | Raw facts in one commit. Refused for `dialog.*`, `dreams.deploy/*`, and protected entities (§10.5). |
| `define` | `rule` or `concept` (formal notation), `path` | Installs a deductive rule, or writes a schema note. |
| `backlinks` | note | Notes that link to it. A canned query, and also a worked example. |
| `neighbors` | note, `depth` (≤ 2) | The link graph around a note, both directions. |
| `history` | note, optional `attribute` | Earlier values and when they were superseded (Dialog history). |
| `changes` | `since` (Unix ms) | Entities whose `dreams.note/updated` is at or after `since`, newest first. |
| `list_conflicts`, `resolve_conflict` | see §6 | |
| `deploy_task`, `disable_task`, `list_tasks`, `run_task` | as in the spike, §10 | |
| `pull_feeds` | as in the spike | |

That is 19 tools, against the spike's 15. `search_docs`, `get_doc`, `put_doc`, `get_rev`, `doc_history`, `delete_doc`, `diff_doc_conflicts` and `resolve_doc_conflicts` collapse into the rows above.

MCP **resources**, **skills** and **prompts** are served as in the spike:
- every note at `dreams:///<path>`, as Markdown;
- skills at `skill://<name>/SKILL.md`;
- prompt entities as MCP prompts;
- changes on the `subscriptions/listen` stream, driven by a Dialog subscription rather than by polling SQLite.

### 8.3 The `query` tool in detail

`rule` is a Dialog deductive rule in formal notation: `deduce`, `when`, optional `unless` and `reduce`.

- dreams evaluates it **once, without installing it**.
- The head's fields are the result columns. By convention they use the scratch domain `dreams.query`, so a one-off query never creates relations that look real.
dreams adds exactly two pieces of shorthand. Every other part of the input goes to Dialog as is.

- **`find`.** `deduce` may be given as `{"find": ["note", "path"]}`. dreams expands it to a head with `dreams.query/<name>` fields, `as` left open.
- **Concepts by name.** A premise may be `{"assert": {"concept": "<name>"}, "where": {…}}`.
  - `<name>` is either a built-in concept name (`note`, `task`, `runner`, `run`, `feed`, `feed-item`, `skill`, `prompt`, `bookmark`) or a schema note path (`schemas/recipe.yaml`).
  - dreams replaces it with the concept's inline `with`, which is the only form formal notation allows.
  - It is accepted in `query`, `define` and task `when`.
  - An installed rule stores the expanded form, so it keeps working if the schema note later changes.
  - Both pieces of shorthand retire if Dialog's abbreviated notation, which can name concepts, gets an expander (§8.4).

Example: notes tagged `dream`, newest first by path.

```json
{
  "deduce": { "find": ["note", "path", "updated"] },
  "when": [
    { "assert": { "with": { "is": { "the": "dreams.note/tag", "as": "text:", "pick": "all:" } } },
      "where": { "this": { "?": { "name": "note" } }, "is": "dream" } },
    { "assert": { "with": { "is": { "the": "dreams.note/path", "as": "text:" } } },
      "where": { "this": { "?": { "name": "note" } }, "is": { "?": { "name": "path" } } } },
    { "assert": { "with": { "is": { "the": "dreams.note/updated", "as": "natural:", "pick": "max:" } } },
      "where": { "this": { "?": { "name": "note" } }, "is": { "?": { "name": "updated" } } } }
  ]
}
```

**Gotcha the skill must teach: picks.** A premise reads an attribute under the pick it declares. A premise that reads `dreams.note/tag` without `"pick": "all:"` reads it under `last:`, sees one tag per note, and silently misses matches. This is the main reason `describe` returns full descriptors to copy.

**Seeded worked examples.** The `dialog` skill (`src/seed/skills/dialog.md`) ships one example for each common shape:

1. notes with a tag (above)
2. backlinks to a note (a `dreams.note/link` premise with `is` bound)
3. notes changed by a writer (`dreams.note/by` == a session DID)
4. notes changed since a time (a `>=` constraint on `dreams.note/updated`)
5. notes with no links in or out (`unless`)
6. tag counts (`reduce` with `count`)
7. conflicts (§6)
8. run receipts of a task, with exit codes
9. dangling links: "notes someone wished existed", a natural seed for dreaming

**How it runs** (`src/notation.rs`, against Dialog `v0.4.0-rc.2`):
1. dreams compiles the descriptor and evaluates the rule's **body** (`DeductiveRule::plan`) as a query whose rows are its raw bindings, without installing anything.
   - Reading the head concept back would be per entity: a head's fields are attributes of `this`, so a row like `(tag, note)` would lose which values went together.
2. Dialog requires every rule to bind `this`. With `find`, dreams renames the first variable found at a premise's `this` to `this`, and maps it back in the result.
3. **`reduce` is folded by dreams**, not Dialog. Dialog at this tag refuses `reduce` in a deductive rule. dreams groups rows by the `find` columns that aren't reduced, and folds each group: absent values are skipped, as in SQL.
4. Constraints inside a variable (`{"?": {"name": "x", "where": {...}}}`) are refused with a hint, because Dialog parses them but ignores them at this tag. Constraint premises (`==`, `<`, `>=`, `text/like`, …) work.

Installed rules (`define`) cannot `reduce`.

### 8.4 Notation, YAML and the abbreviated form

`query` and `define` accept formal notation as JSON, or as YAML (the two are one to one).

Dialog's abbreviated notation (`?var`, `_`, punning) is far easier for an LLM to write. dreams accepts it **as soon as Dialog ships an expander**, and does not write its own. Until then, the skill teaches the formal form. It is verbose, but every field is explicit, so an agent's mistakes are visible.

---

## 9. Search sidecar

Dialog has no full-text search. dreams keeps a **Tantivy** index in `index/`:

- **One document per note entity.** Fields: `entity` (stored), `path` (stored, raw string), `title`, `content`, `tags`, and `updated` (fast field).
- **Ranking.** BM25 with field boosts carried over from the spike's `bm25(docs_fts, 10.0, 1.0, 5.0)`: title ×10, tags ×5, content ×1.
- **Snippets.** Snippets come from `SnippetGenerator` on `content`, with `**` highlighting as in the spike's `content_matches`.
- **Filters.** `tag` and `prefix` filters are term and prefix queries on `tags` and `path`. An empty `q` lists notes newest first by `updated`, replacing the spike's `list_docs`.
- **Freshness: catch up on read.** Every write through dreams stamps `dreams.note/updated` on each entity it touches (§3.2), so `(entity, updated)` pairs identify what changed. Before a search, dreams compares the vault's pairs, from one query, with the pairs stored in the index. It then re-indexes the entities that differ, drops the ones that are gone, and commits.
  - Tantivy's own writer lock serializes writers. A searcher that can't get the lock searches the slightly stale index.
  - No daemon is required for search to be correct.
  - Dialog has a tree diff (`differentiate`) that could replace the full comparison, but it isn't public API.
- **Derived.** The index never syncs. `dreams reindex` deletes and rebuilds it.

Embeddings, when they come, go in this same sidecar.

---

## 10. Agents: tasks, runners, runs

The spike's task model was its most worked-out part (`docs/proposals/scheduled-tasks.md`, `src/task.rs`). It carries over, except for **triggers**, which datalog changes.

### 10.1 Runners

Runners carry over unchanged in behavior:

- `command` is an argv array, run without a shell.
- The tokens `{vault} {task} {run} {mcp} {out} {exe}` are substituted.
- The prompt goes to stdin. The last message is read from stdout, or from `{out}`.
- The environment includes `DREAMS_VAULT`, `DREAMS_TASK`, `DREAMS_RUN`, `DREAMS_SESSION` and `DREAMS_MCP`.

The seeded runners are `claude`, `codex`, `pi` and `feeds`.

### 10.2 Triggers are queries

| Spike | Here |
|---|---|
| `every` alone: fire each interval | Same. |
| `every` + `when: {glob, tag, type, ids}`: fire if a matching doc changed | `every` + `when`: a **query**. Fire if its result rows changed since the last run; `every` is the minimum gap. |

`when` holds a formal-notation rule. On each tick, for each due task with a `when`, the daemon:
1. re-runs the query;
2. sorts the rows;
3. compares them with the rows saved at the last run (`dreams.deploy/rows`, local repo, as JSON text).

The task fires when the sets differ. The changed rows are appended to the prompt as "Changed since last run:" followed by one line per added or removed row, which replaces the spike's changed-document list.

This survives daemon restarts and other processes' writes. Dialog subscriptions can't be serialized, and they hold their head in memory, so they are only an optimization for a long-running daemon. Leave them for later.

The spike's filters become one-line queries, which `task add` keeps as flags:
- `--tag inbox` → the tag premise
- `--glob 'notes/*'` → a `dreams.note/path` premise plus the `Like` formula
- `--type task` → the Task concept

Arbitrary triggers then come for free:
- "When a note gains a dangling link"
- "When a feed item from a feed tagged `research` arrives"
- "When a daily note has an `intention` but no `## Brief`"

### 10.3 Deploy and confirmation (local repo)

A task document syncs, but it runs only where it is deployed. Deploy state lives in the **local repository**, which never gets a remote. It is a second Dialog repository, which is simpler than a side file and uses the same API.

| Attribute (local repo) | Meaning |
|---|---|
| `dreams.deploy/task` | The task entity |
| `dreams.deploy/hash` | Hash of the task's and runner's facts as confirmed (canonical CBOR of their sorted facts). Replaces the spike's pinned `task_rev` and runner rev. |
| `dreams.deploy/enabled` | `boolean:` |
| `dreams.deploy/enabled-at`, `last-run` | `natural:` (Unix ms) |
| `dreams.deploy/query`, `rows` | The `when` query as deployed, and its rows at the last run. A redeploy keeps the rows unless the query changed. |
| `dreams.deploy/lease-until` | `natural:` |
| `dreams.deploy/requested` | `boolean:` (from `run_task`) |
| `dreams.deploy/pending` | The run in progress (receipt path, start, rows to record), for crash recovery |
| `dreams.deploy/session` | The DID the task's runs write as |

- **Confirmation.** `deploy_task` shows the exact prompt, command, `cwd`, `when` query and timeout, and asks the person through MCP elicitation, as in the spike. If the task changes while the question is open, it asks again.
- **Changes after deploy.** A task whose current hash differs from its deployed hash shows as `changed` and does not run until it is redeployed. This covers edits that arrive by sync.
- **Who may deploy.** Agents cannot deploy.

### 10.4 Sessions and no self-wake

Each run writes as its own **session DID**, derived by Dialog from the device's peer key and the task's entity id (`--session`, `DREAMS_SESSION`), so every run of one task writes as the same DID. dreams stamps `dreams.note/by` with it on every write, the receipt included.

The spike's no-self-wake rule becomes a filter: rows whose entities' latest `by` is the task's session are dropped from the change set. Notes written by an agent can be queried with `dreams.note/by`, which the spike's `_actor` could do only through SQL.

### 10.5 Protected entities

These carry over from the spike:
- Seeded runners, seeded schema notes and run receipts are read-only over MCP. `doc resolve --auto` runs the seeded runners without a deploy, so an agent must not be able to rewrite them.
- Agents may create new runners, because nothing runs until a person deploys it.

dreams marks protected entities with `dreams.note/protected = true`. `transact`, `put_note` and `delete_note` refuse to touch them over MCP. The CLI can, with `--force`.

### 10.6 Receipts, leases and the daemon

- **Receipts.** Each firing writes a Run entity at `runs/<task-path-stem>/<uuidv7>.md`. Receipts sync.
- **Leases.** Before the agent starts, the scheduler takes a lease in the local repo: the runner's timeout plus one minute. One commit to each repository finishes a run: the receipt goes to the vault, and the cursor moves and the lease is released in the local repo. They are two commits because they are two repositories.
- **Leases are a compare-and-swap.** The lease commit is tried once on the head the scheduler read; if another process moved the local head, it reads and decides again.
- **Crash recovery.** If dreams crashes between the two commits, the next tick finds the receipt that `pending` names and records the run without re-running.
- **The daemon.** `dreams daemon` ticks every `--interval`, and also wakes when the vault's head moves. `daemon install` writes a launchd or systemd unit, as in the spike.

---

## 11. Feeds, skills, prompts

**Feeds** port one to one as entities (§5.1). The one datalog win: a feed item's entity id is **content-derived** from `(feed entity, item key)`. Two devices pulling the same feed converge on the same entities instead of creating duplicates that the spike would have had to merge.

- **Dedup.** Deleting an item retracts everything but `dreams.feed-item/feed` and `dreams.feed-item/key`, which are kept as the "seen" marker. A pull never re-creates an item whose key fact exists.
- **Item text is data.** Feed item text is data, not instructions. Agents read the feed's `instructions` before they process its items. The rule appears in the MCP server instructions and the `dreams` skill, as in the spike.
- **Fetching and writing are separate.** Network fetches happen with no repository commit in progress, as in the spike's gather and apply split.

**Skills and prompts** are entities with `dreams.skill/name` or `dreams.prompt/name`, served as in the spike. When two have the same name, the one with the greatest `updated` wins.

**Seeded content:**
- skills:
  - `dreams`: how the vault works, tasks, runners and feeds. It points to `dialog` for queries.
  - `dialog`: queries, writing facts, open-ended concepts and rules (§5.5, §8.3).
  - `daily-note`, `bookmark`, `brief`.
- prompts: `daily`, `intention`, `bookmark`, `brief`
- dormant tasks: `tasks/brief.json`, `tasks/pull-feeds.json`

Seeding asserts only facts whose values differ from the defaults. Seeded entities get content-derived ids from their paths, so two devices that seed independently converge instead of conflicting. This fixes the spike's known problem where vaults seeded by different binary versions conflicted on sync.

---

## 12. Identity, sync and multiplayer

### 12.1 Identity

Dialog's model is used as is: Ed25519 `did:key`s with UCAN delegation chains.

| Dialog | dreams |
|---|---|
| Repository DID | The vault's identity. |
| Authority | The person. One key, held on their first device, or sealed and moved by `dreams device add`. |
| Peer | Each device running dreams. |
| Session | A run of the CLI or MCP server, or one agent run (§10.4). |

### 12.2 My devices

The vault repository's `main` branch pushes to and pulls from a remote:

```
dreams remote add origin s3://bucket/dreams     # or a folder, or a UCAN service URL
dreams sync                                     # pull, then push
```

- **Push is fast-forward only.** `sync` always pulls first. A rejected push is retried after another pull.
- **Pull merges.** Concurrent edits become conflicts (§6), not errors.
- **Automatic sync.** The daemon syncs on an interval when a remote is configured.
- **Remote types.** A folder remote, such as a NAS or a Syncthing'd directory, is the "gloves" default. S3/R2 or a UCAN access service is for people without one.
- **Folder sync is now safe.** The spike warned against putting a live vault in Dropbox or iCloud. A Dialog *remote* is designed for "dumb" shared storage, because it uses blocks plus compare-and-swap. The live `dialog/` folder still must not be in Dropbox.

### 12.3 Other people's vaults

**Petnames** are facts in the vault, so they sync across my devices:

| Attribute | |
|---|---|
| `dreams.peer/petname` | `"alice"` |
| `dreams.peer/repository` | Alice's vault DID |
| `dreams.peer/remote` | Where to pull it from |
| `dreams.peer/access` | The UCAN delegation granting read access (`text:`, encoded) |

**Following:**

```
dreams peer add alice <remote> --repository did:key:… --access <ucan>
dreams peer pull alice            # or lazily, on first read
```

Alice's vault is opened as a separate, read-only Dialog repository replica in `dialog/`. Dialog's lazy hydration fetches only the blocks a query touches, so following a large vault is cheap until it's read.

**`dreams://` URIs:**

| URI | Resolves to |
|---|---|
| `dreams:///notes/x.md` | This vault, path `notes/x.md` |
| `dreams://alice/notes/x.md` | Petname `alice`, then that repository, then the note whose `dreams.note/path` is `notes/x.md` |
| `dreams://alice/did:key:z6Mk…` | A specific entity in Alice's vault |

The MCP tools take an optional `vault` (a petname); the default is this vault. `get_note`, `query`, `search` and `describe` work against a followed vault.

Queries do **not** join across vaults. Each query runs against one repository. An agent that wants to relate Alice's notes to mine queries both and writes `dreams.note/xlink` facts. That is a dreaming task in its own right.

**Publishing.** What Alice can read is her whole `vault` repository, if she delegates read on it. There are no per-note permissions; UCAN grants apply per repository. So "public notes" means a second repository:

```
dreams publish notes/x.md     # copy the note's facts into the `public` repository
```

`public` has its own remote and its own read delegation, which Alice hands out.

> Open question: copy, or move with a cross-repository link back? Copying is simpler: a published note is a snapshot, and `publish` again updates it.

### 12.4 Gaps to state plainly

- **No encryption on remotes.** Dialog stores nodes unencrypted on remotes for now, and encryption is planned upstream. Until it lands, a remote must be storage the person trusts. `dreams remote add` says so.
- **Revocation is incomplete upstream.** A delegation handed out can't yet be reliably revoked. Until revocation lands, rotating the remote's storage credentials is the way to cut access.

---

## 13. CLI

Global flags: `--vault` (env `DREAMS_VAULT`) and `--json`. `--actor` is gone; the session DID replaces it.

| Spike | Here |
|---|---|
| `init`, `restore-defaults`, `serve`, `mcp-json` | Same |
| `doc put/update/get/delete/list/search/history` | `note put/get/rm/list/search/history` |
| `doc conflicts/diff/resolve` | `note conflicts/diff/resolve` |
| `doc changes` | `changes --since` |
| — | `query <file|->` (notation), `describe [domain]`, `rule add/list/rm`, `doctor`, `reindex` |
| `import`/`export <dir>` | Same, plus `--domain` (§7) |
| `pull`/`sync <peer.db>` | `remote add/list/rm`, `pull`, `push`, `sync` |
| — | `peer add/list/rm/pull`, `publish`, `device add` |
| `task add/list/deploy/disable/rm/check/run/runs` | Same. `task add --when <query-file>` alongside `--tag`, `--glob` and `--type`. |
| `runner …`, `feed …`, `tick`, `daemon […]` | Same |

---

## 14. Implementation shape

- **One crate, one binary.** Edition 2024.
- **Modules**, following the spike's split where it held up:

  | Module | Role |
  |---|---|
  | `cli` | Command-line interface |
  | `mcp` | MCP server |
  | `vault` | Open the repositories, sessions, the commit helper |
  | `note` | Markdown view, links, protected entities |
  | `concepts` | Built-in attributes and concepts |
  | `notation` | Parse, `find` expansion, describe |
  | `import`, `export` | §7 |
  | `search` | Tantivy |
  | `resolve`, `diff` | Ported from the spike |
  | `task`, `runner`, `daemon` | §10 |
  | `feed`, `skill`, `prompt`, `seed` | §11 |
  | `peer` | Petnames, remotes |

- **Dependencies:**
  - Dialog is git-only, not on crates.io. Every crate is pinned to one tag (`v0.4.0-rc.2`): `dialog-artifacts`, `dialog-capability`, `dialog-credentials`, `dialog-effects`, `dialog-peer`, `dialog-query`, `dialog-repository`, `dialog-storage`, `dialog-varsig`, and `dialog-remote-fs` in M3.
  - Opening a vault follows Dialog's tests, not its READMEs, which are stale:
    1. keys from a `CredentialStore` at the vault folder: `system`, `peer` and `custodian`;
    2. `Peer::new(..).at(..).space(..).with(storage).grant(Allowance::storage(..)).base(..)`;
    3. onboard an `account` vault once;
    4. one session per process.
  - Everything else: rmcp, clap, tokio, tantivy, feed-rs, html2text, ureq, diffy, serde, serde_json, serde_yaml_ng, uuid, thiserror, tracing.
  - No SQLite and no `jsonschema`.
- **Async throughout.** Dialog's API is `.perform(&operator).await`. The spike's synchronous `Store` behind a `Mutex` becomes one `Vault` handle shared across tasks.
- **Multiple processes.** The CLI, the MCP server and the daemon may open the vault at the same time.
  - Dialog's filesystem branch head is a compare-and-swap under a PID lock with an atomic rename, which is safe across processes on one host.
  - A stale handle's commit is refused, and a busy lock fails at once rather than waiting.
  - dreams wraps every commit in refresh-and-retry with backoff. It re-applies the transaction on the new head, since assertions don't depend on a parent revision.
  - PID locks aren't safe on network filesystems, so the live `dialog/` folder must be on a local disk.

---

## 15. Risks and open questions

**Risks**

1. **Dialog churn.** It is experimental, with breaking changes every few days. Pin a git rev. These are the Dialog surfaces dreams depends on, which are the upgrade checklist:
   - attribute and concept derives
   - `DeductiveRuleDescriptor` serde
   - one-off rule evaluation (`query().with(rule)`)
   - claims selection
   - history scan
   - fs storage and its cross-process CAS
   - the fs, S3 and UCAN remotes
   - `EntityExt::of`
   - picks

   `tests/dialog_smoke.rs` checks all of these except the remotes.
2. **Notation ergonomics for LLMs.** Formal notation is verbose. Mitigations: `describe` descriptors to paste, the `find` and `concept` shorthands, the `dialog` skill's worked examples, and errors that teach. Measure how often agents get queries wrong. If it's often, adopting Dialog's abbreviated notation is the next step, not a new language.
3. **Large text.** Content over 4 KiB spills to assets. Tantivy indexing reads the full value, which is fine locally but means a hydration fetch for followed vaults.
4. **`describe` cost.** Counting facts per attribute scans AEV. Cache the counts with the index and refresh them on the same catch-up.

**Open questions**

- Publish: copy or move? (§12.3)
- Revisit history and search catch-up when Dialog's tree diff becomes public. Both are full scans today (§9).
- Should `dreams.note/by` hold the session DID, or the more stable peer DID plus a task entity? (§10.4)

---

## 16. Milestones

1. **Substrate.** Vault and local repositories, the note model, built-in concepts, `import`/`export` (including spike migration), Tantivy search, and the MCP tools `describe`, `query`, `search`, `get_note`, `put_note`, `delete_note`, `transact`, `define`, `backlinks`, `neighbors` and `history`. Also the `dreams` skill with worked examples.
   - *Done when:* a migrated spike vault can be explored by Claude Code over MCP using only `describe` and `query`.
2. **Dreaming.** Runners, tasks with query triggers, deploy confirmation, receipts and leases, the daemon, feeds, skills and prompts, conflicts and resolve.
   - *Done when:* the seeded brief and feed tasks run on the daemon, and a `when` query trigger fires on a dangling link.
3. **Multiplayer.** Remotes and `sync`, device add, petnames, `dreams://` resolution, `publish`.
   - *Done when:* two laptops sync through a folder remote, and one follows the other's `public` repository and links to a note in it.
