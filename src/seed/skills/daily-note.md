---
dreams.skill:
  name: daily-note
  description: "Create, add to, or find daily notes: one note per day, with the date as its path. Use when the user mentions today's note, a daily note, a journal, or a log for a day."
---
# Daily notes

A daily note is one note for one day. Its path is the local date as `YYYY-MM-DD.md`, for example `2026-09-23.md`. The `.md` extension is part of the path. It has the tag `daily`.

A daily note has two parts:
- `content` is the log for the day. New text goes at the end.
- `dreams.daily/intention` is the one intention for the day. A new intention replaces the old one.

## Add to a daily note

1. Find the date. For today, use today's local date. For another day, use that date. If you do not know the date, ask the user. The path is the date plus `.md`.
2. Call `get_note` with the path.
3. If the note is not found, create it. Call `put_note` with `path`, `title` set to the date, `content` set to the new text, and `tags` set to `["daily"]`.
4. If the note is found, update it. Call `put_note` with `path`, `expect_updated` set to the note's `updated`, and `content` set to the old content plus the new text:
   - If the old content is empty, use the new text.
   - If not, remove the newlines at the end of the old content. Then add `\n\n`, then the new text.
   - If the note's tags do not contain `daily`, also send `tags` with `daily` added to its tags.
   - Send no other fields. Fields you leave out do not change.
5. If `put_note` fails because the note is stale, the note changed after you read it. Go back to step 2.

Write `content` as Markdown. Do not change or remove earlier text unless the user asks.

## Set the intention

Call `put_note` with `path` and `fields` set to `{"dreams.daily/intention": "<text>"}`. If the note is new, also send `title` set to the date and `tags` set to `["daily"]`. Do not send `content`.

## Find daily notes

- To list daily notes, call `search` with no `q` and `tag` set to `daily`. The newest changes come first.
- To find a daily note by its text, call `search` with `q`.
