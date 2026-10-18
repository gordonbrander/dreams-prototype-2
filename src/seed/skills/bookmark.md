---
dreams.skill:
  name: bookmark
  description: "Save a web page as a bookmark: fetch it, summarize it, and tag it. Use when the user wants to bookmark, clip, or save a link, or find saved links."
---
# Bookmarks

A bookmark is one note for one web page. It has the tag `bookmark`, and matches the concept `bookmark`.

A bookmark has these fields:
- `dreams.bookmark/url` is the address of the page, as the user gave it.
- `title` is the title of the page.
- `content` is a short summary of the page, then the notes from the user.
- `tags` contains `bookmark` and some topic tags.

## Make the path

The path of a bookmark is `bookmarks/<origin-slug>/<path-slug>.md`. All bookmarks from one site thus share the prefix `bookmarks/<origin-slug>/`. Make the slugs from the URL with these steps:

1. Make all letters lowercase.
2. Remove the scheme, for example `https://`.
3. Remove `www.` at the start.
4. Remove the query and the fragment: all text from the first `?` or `#`.
5. Divide the rest at the first `/`. The text before it is the origin: the host and the port. The text after it is the path.
6. In the origin and in the path, replace each sequence of characters that are not `a`-`z` or `0`-`9` with one `-`. Then remove `-` at the start and at the end.
7. If the path slug is empty, use `index`.

Examples:
- `https://www.example.com/foo/bar?x=1` gives `bookmarks/example-com/foo-bar.md`.
- `http://Example.com/` gives `bookmarks/example-com/index.md`.
- `https://blog.example.org/2026/09/my_post.html#part-2` gives `bookmarks/blog-example-org/2026-09-my-post-html.md`.
- `http://localhost:8080/docs` gives `bookmarks/localhost-8080/docs.md`.

## Save a bookmark

1. Make the path from the URL.
2. Get the page with your web fetch tool. Find its title and read its text. If you cannot get the page, use the URL as the title, and tell the user.
3. Write the summary: two to five sentences or bullets in Markdown about what the page says.
4. Call `get_note` with the path.
5. If the note is not found, create it. Call `put_note` with `path`, `concept` set to `bookmark`, `title`, `content`, `tags`, and `fields` set to `{"dreams.bookmark/url": "<url>"}`.
   - `content` is the summary. If the user gave notes, add `\n\n## Notes\n\n` and then the notes.
   - `tags` is `bookmark` and one to five topic tags. Write tags in lowercase, with `-` between words. Use tags that other bookmarks use when they fit. To see them, call `search` with no `q` and `tag` set to `bookmark`.
6. If the note is found, update it. Call `put_note` with `path`, `expect_updated` set to the note's `updated`, and:
   - `title`, and `fields` with the new URL.
   - `content`: replace the summary with the new summary. Keep the `## Notes` section. Add the new notes to the end of it, with one empty line between notes.
   - `tags`: the old tags, and new topic tags if they fit. Make sure that it contains `bookmark`.
7. If `put_note` fails because the note is stale, the bookmark changed after you read it. Go back to step 4.
8. Tell the user the title, the tags, and the path.

## Find bookmarks

- To find the bookmark for a URL, make the path from the URL. Then call `get_note` with the path.
- To list bookmarks, call `search` with no `q` and `tag` set to `bookmark`. The newest changes come first.
- To list the bookmarks from one site, make the origin slug from its URL. Then call `search` with no `q` and `prefix` set to `bookmarks/<origin-slug>/`.
- To find bookmarks about a subject, call `search`. It searches the title, content, and tags. It does not search the URL.
