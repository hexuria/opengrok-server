# Heading 1
## Heading 2
### Heading 3
#### Heading 4
##### Heading 5
###### Heading 6

Setext heading one
==================

Setext heading two
------------------

## Inline

A paragraph with **bold**, *italic*, ***bold italic***, ~~strike~~, `inline code`, a [link](https://example.com), a [link with title](https://example.com "Barok Works"), an autolink <https://example.com>, a reference link [Barok][ref], and an image: ![a mock image](https://upload.wikimedia.org/wikipedia/commons/3/3a/Cat03.jpg).

Escaped characters: \*not italic\*, \`not code\`, \# not a heading. An entity: &copy; &amp; &lt;tag&gt;. A hard line break follows this line  
and this is the next line. Unicode: café — naïve — 日本語 — emoji 🎉.

[ref]: https://example.com

## Chips

Jump: [jump back](sand-msg:t1u) · Settings: [Theme](grokbot://app/v1/settings?id=theme) · Plugin: [Notion](grokbot://app/v1/plugin/add?id=404) · Workflow: [deploy](sand-workflow:deploy-prod)

## Lists

- a bullet
- another bullet
  - a nested bullet
    - a doubly nested bullet
- a third

1. first
2. second
   1. a nested number
   2. another
3. third

- [x] a done task
- [ ] an open task
  - [x] a nested done task

* asterisk bullet
+ plus bullet

## Quotes

> A blockquote, with a second line
> to prove the wrap.
>
> > A nested blockquote.

## Code

Indented code block:

    fn indented() { println!("four spaces"); }

Fenced without a language:

```
plain fence, no highlighting
```

Fenced TypeScript:

```ts
export async function describe(name: string): Promise<Fixture> {
  const path = `${FIXTURE_DIR}/${name}`;
  const bytes = await readFile(path);
  const kind = /\.(png|mp4|pdf)$/.exec(name)?.[1] ?? "text";
  return { name, size: bytes.length, kind };
}
```

Fenced Rust:

```rust
pub fn describe<'a>(name: &'a str, bytes: &[u8]) -> Fixture<'a> {
    println!("{name}: {} bytes", bytes.len());
    Fixture { name, size: bytes.len() }
}
```

Fenced JSON and shell:

```json
{ "fixture": "mock-notes", "ok": true, "n": null }
```

```sh
cargo test -p opengrok-server --lib mock_fixtures
```

## Tables

| Left      | Centre                      | Right |
|:----------|:---------------------------:|------:|
| DoorProbe | openai/gpt-5.5              |    12 |
| Hexuria   | anthropic/claude-sonnet-4.5 |     7 |
| New chat  | xai/grok-4.6                |     0 |

| Fixture | Reader      | Draws | Notes with `code` and **bold** |
|---------|-------------|:-----:|--------------------------------|
| pdf     | PDF viewer  |  yes  | three pages                    |
| docx    | mammoth     |  yes  | every run style                |
| html    | text reader |  yes  | must not execute               |

## Rules

---

***

___

## Footnote

A sentence with a footnote.[^1]

[^1]: The footnote text, at the bottom.

Written by the mock fixture catalogue.
