+++
title = "Uncle Bob's uml-viewer on Rust: 35 dependency cycles down to 3"
date = "2026-09-22"
slug = "uml-viewer-rust-dependency-cycles"
excerpt = "I fed my Rust codebase to Uncle Bob's uml-viewer and let an AI agent fix what the red arrows showed. Six commits: 35 cycles to 3, 167 violations to 109."
tags = ["ai-agents", "rust", "architecture", "clean-architecture"]
draft = false
og_image = "/static/marketing/og-uml-viewer-rust.png"

[[faqs]]
q = "What is uml-viewer?"
a = "uml-viewer is an open-source desktop tool by Uncle Bob (Robert Martin) that draws a codebase as a UML-like diagram you can click. Namespaces are components, modules are their elements, and a dependency that points from an inner layer to an outer layer is drawn red. It is built to be used together with a coding agent: you look at the diagram, tell the agent what to change, and the diagram is redrawn."

[[faqs]]
q = "Does uml-viewer work with Rust?"
a = "Not directly. The only parser it ships today reads Clojure. The diagram is drawn from a plain EDN file that lists modules, dependency edges and layer levels, so for Rust I wrote a small script that walks the source tree and emits that file. Any language works the same way if you can list its modules and their imports."

[[faqs]]
q = "What is the Dependency Rule?"
a = "The Dependency Rule from Clean Architecture says that source code dependencies must point inward, toward the parts of the system that change least. Your domain types should not import your HTTP handlers. In uml-viewer you list which modules sit at which level, and every arrow that points the wrong way is drawn red."

[[faqs]]
q = "How do you find circular dependencies between Rust modules?"
a = "The compiler will not tell you, because Rust allows two modules in one crate to import each other. You have to build the module graph yourself: one node per file, one edge per use path that starts with crate, super or self, then look for pairs that point at each other. In my codebase that turned up 35 such pairs, most of them between storage, quotas, api and web."

[[faqs]]
q = "Why not just ask the AI agent to refactor the codebase?"
a = "Because that instruction has no test. The agent will move things and call it done, and you have no way to say whether the result is better. A diagram gives you one red arrow at a time, and after each change you can see whether that arrow is gone and whether a new one appeared. The agent does the moving and the picture does the judging."
+++

> **TL;DR.** Uncle Bob released [uml-viewer](https://github.com/unclebob/uml-viewer), a clickable architecture diagram that draws Dependency Rule violations in red and is meant to be driven together with a coding agent. It only parses Clojure, so I wrote a 200-line script that turns my Rust crate into its input file. The first picture showed 167 red arrows and 35 dependency cycles. Six commits and 315 files later: 109 red arrows, 3 cycles, and `app.rs` down from 1,087 lines to 642. No behavior changed. The tool matters less than the loop it forces on you: look, point at one arrow, let the agent move code, look again.

Uncle Bob's argument for the tool is short: agents still need supervision, and reading every line they write is the bottleneck. So supervise the structure instead of the text: draw the system, find the shape that is wrong, tell the agent to fix that shape, and check the new drawing.

I wanted to know if that works on a real codebase, so I pointed it at [Uptimepage](https://github.com/uptimepage/uptimepage), about 146,000 lines of Rust in one crate. It does, with one condition: the picture is only as honest as the input file you feed it.

![Two panels of five stacked layers, vocabulary at the bottom and assembly at the top. Before: five red arrows point up from storage and config into api, worker, auth and billing, and two grey loops mark cycles between api and web and between storage and quotas. After: the same modules plus dashed green boxes for request, templates, security and pagination, and every arrow points down or sideways.](/static/marketing/blog-uml-viewer-layers.webp)

*Five of the 167 red arrows and two of the 35 cycles, and where the code went. Every arrow that pointed up now points down.*

## What uml-viewer is

uml-viewer is an open-source desktop tool by Uncle Bob (Robert Martin) that draws a codebase as a UML-like diagram you can click. It is written in Clojure and needs the Clojure CLI and Java 21 or newer.

Namespaces are components. The modules inside a namespace are the component's elements. Nesting can go as deep as your source tree does. You double-click a component to open the next level, double-click a module to see its class card, and click a function to open the source file at that line.

Color comes from CRAP and mutation scores, so a red box is code with high complexity and weak tests, or code with no metrics at all, which the README counts as the worst grade. Red arrows come from the Dependency Rule: you tell the tool which namespaces sit at which architectural level, and every dependency that points from an inner level to an outer level is drawn red.

The tool is built to run next to an agent. By default it opens a tmux window with Grok in the examined project and the two talk through a small mailbox directory. You can also drive it by hand from any agent session: regenerate the input file, press R in the viewer, and the diagram reloads.

## Getting a Rust crate into it

The only parser it ships reads Clojure. That sounded like the end of the experiment, until I read what the viewer actually consumes. It never sees source code. It reads one EDN file: a list of classes with a namespace and a level, a list of edges with a from, a to and a kind, and a list of levels. That is a format any script can emit.

So I asked the agent to write a generator for Rust. It came out at about 200 lines of Python and does four things:

- Every `.rs` file is one class. `mod.rs` stands for its directory and `lib.rs` is skipped.
- Every `use crate::…`, `super::…` and `self::…` path, plus every inline `crate::a::b` path in a function body, becomes a dependency edge to the nearest enclosing module that exists as a file.
- Comments are stripped first, so a path mentioned in a doc comment does not count as a dependency.
- A hand-written `LEVELS` list gives each top-level module a rank.

The last item is the only opinion in the script:

```python
# inner (high level) first, matching the Dependency Rule ranks
LEVELS = [
    ["domain", "error", "text", "metric_names"],
    ["storage", "security", "net", "http_client", "config",
     "quotas", "observability", "pagination"],
    ["pipeline", "escalation", "notifier", "email", "telegram",
     "whatsapp", "billing", "auth", "analytics",
     "jobs", "http_outbound", "targets"],
    ["api", "web", "mcp", "marketing", "agent", "worker", "scheduler",
     "request", "public_status", "templates", "oauth",
     "ad_hoc_dispatch", "channels"],
    ["app", "router", "bootstrap", "main", "bin"],
]
```

Rank 0 is vocabulary that everyone may use: domain types, error codes, text helpers. Rank 1 is infrastructure: storage, security primitives, config. Rank 2 is the services that do the work: probing, escalation, notification, billing. Rank 3 is every way into the system: HTTP handlers, HTML views, the MCP server, the marketing site. Rank 4 is assembly: the app state, the router, main.

The rule is then mechanical. An edge is a violation when the module it comes from has a smaller rank than the module it points to. Same rank is allowed. Foreign crates like axum and sqlx are ovals outside the diagram and are never compared.

> **The picture is only as honest as the levels list**
>
> The generator does not decide the architecture. I do, in that list. If you put `api` in the same group as `domain`, the diagram turns green and you have learned nothing. Write the list you want to be true, then let the red arrows show how far the code is from it.

## What the first picture showed

The first diagram had 167 red arrows, 35 pairs of modules that imported each other, 17 modules importing `api`, and an `app.rs` of 1,087 lines.

Behind the numbers were shapes I half knew about and had never seen drawn:

- `storage` reached up into `api` for error codes and dashboard read models, and into `worker` for heartbeat state.
- `api` and `web` imported each other. The JSON side took session and token extractors, cookies and client IP from the HTML side, and the HTML side took the heartbeat read model from the JSON handlers.
- `quotas` read accounts and organizations from `storage`, while `storage` embedded quota SQL fragments and read plan types from `quotas`. A cycle in both directions.
- `marketing` and `oauth` imported `web` for three template filters.
- `config` imported `auth` and `billing` for two enums.

None of this was visible in the review of any single pull request. Every one of those imports was reasonable on the day it was written. A diff shows one import at a time, so the sum of them never appeared in any review.

## The loop

Every round had the same six steps:

1. Regenerate the EDN file and reload the viewer.
2. Pick one red arrow. Hover it to see which module pairs it bundles.
3. Tell the agent one thing: what must not import what, and where the shared piece should live.
4. The agent moves the code and runs the compiler and the tests.
5. Regenerate. Check that the arrow is gone and count the new ones.
6. Run the full suite, review the diff, commit.

The instruction in step 3 is the part that makes this work. It looks like this:

```
storage must not import api. The error codes and the dashboard read models
it takes from there are crate-wide vocabulary. Move the codes to error::codes
and the read models to domain::metrics, and point every reader at the new
path. No behavior changes.
```

That is one rule, one violation, and one place to put the result. The agent does not have to guess what "cleaner" means. It has a named arrow to remove, and the next diagram says whether it did.

I ran this with Claude Code, but nothing in the loop depends on it. Codex, Cursor, Grok, or any agent that can edit files and run a test suite gets the same instruction and the same picture afterwards.

Six rounds took the crate from 167 red arrows to 109:

| Round | Red arrows I pointed at | Where the code went |
|---|---|---|
| 1 | `storage` into `api` and `worker`, `config` into `auth` | error codes to `error::codes`, 12 read models to `domain::metrics`, provider enums to `domain::credential` |
| 2 | `storage` into `auth` and `api` | token hashing, HMAC and SHA-256 helpers to `security`, redaction scrubbers to `security::redaction` |
| 3 | `api` and `web` into each other | extractors, cookies, client IP and host resolution to a new `request` module that imports neither |
| 4 | `storage` and `quotas` into each other | SQL fragments to `storage::count_sql`, plan types to `domain::quota` |
| 5 | `marketing`, `oauth` and `api` into `web` | template filters and formatters to `templates`, `app.rs` split into `config::boot`, `observability::readiness`, `targets::status` and `net` |
| 6 | `public_status` and `web` into `api`, `security` into `http_client` | page envelopes to `pagination`, `Cipher::from_config` into `security` |

And the totals:

![Four paired horizontal bars, red for before and green for after: Dependency Rule violations 167 to 109, module pairs importing each other 35 to 3, modules importing api 17 to 4, lines in app.rs 1,087 to 642.](/static/marketing/blog-uml-viewer-before-after.webp)

*Six commits, 315 files, same test suite before and after. Violations down 35 percent, cycles down 91 percent.*

| | Before | After |
|---|---|---|
| Dependency Rule violations | 167 | 109 |
| Module pairs importing each other | 35 | 3 |
| Modules importing `api` | 17 | 4 |
| Lines in `app.rs` | 1,087 | 642 |
| Files touched | | 315 |
| Behavior changes | | 0 |

I regenerated the graph at every one of the six commits to see what each round removed:

![A line chart over seven points from start to round six. Module pairs importing each other fall 35, 28, 23, 19, 16, 8, 3. Modules importing api fall 17, 11, 9, 6, 6, 6, 4. Under each round a short label names what moved.](/static/marketing/blog-uml-viewer-rounds.webp)

*Cycles and api importers per round. The first two rounds did most of the work on the red count; rounds three to six were about the cycles.*

The shape surprised me. The red arrow count stopped moving after round two. Rounds three to six barely touched it, but they took the cycles from 23 down to 3, because a cycle between two modules at the same level is not a Dependency Rule violation at all. The rule catches arrows that point up and says nothing about two modules on the same level that import each other, so you need both counts.

Not every move survived. In a later round the agent moved the health-check paths into `observability`, and a coupling test on the marketing module said no, because that module is only allowed to reach a short list of leaf modules. Another move put the strict JSON parser under `request`, then had to come back because that parser reads the OpenAPI document. Both reverts took minutes, because the diagram and the test said so before the commit did.

## Why I stopped at 91

After the six commits, 109 red arrows were left. Most pointed into `app`, but ten did not: a sampler and a silence job that lived under `observability` but reached into the scheduler and the notifier, two background jobs reading `public_status`, the HTTP metrics layer, and the rate-limit middleware in `quotas` reading the app state. That middleware was also the third of the three cycles. One more round moved each of those into the module that owns it and left 91 red arrows and 2 cycles.

Every one of the 91 is the same shape. Handlers read the app state, and the app state is built from the modules those handlers live in. `app` imports `api` and `web` to mount them; `api` and `web` import `app` to get the state.

Fixing it is possible. Split the state into per-handler sub-states and hand each handler only its slice. I counted what that would touch: about 90 files of plumbing that make the code harder to read, not easier. I stopped because I could name every remaining arrow, and at that point the diagram had told me everything it was going to.

> **Stop when every arrow has a name**
>
> Drive the count down until every arrow that is left has a name and a reason, then stop. The 91 arrows left in my crate are all "this handler reads the app state", and a diagram that shows them is more honest than one that hides them behind 90 files of indirection.

## Keeping it fixed

Two things stop the graph from drifting back.

The marketing module has a test that is an allow-list: the exact set of leaf modules it may import. A new reach into the app fails the build. It used to be a deny-list, which only catches the mistakes you already thought of.

The other is a habit, not a test. Regressions arrive with feature commits, not with refactors. After every push I regenerate the graph and read the list of red arrows that do not point into `app`. If the list is empty, the feature stayed in its layer. If it is not, the offending edge is one instruction away from gone, before the next feature builds on it.

## Why this beats "refactor the codebase"

Two months ago I wrote about [mapping this codebase for humans and AI agents](/blog/map-your-codebase-for-ai-agents). The lesson then was that a model is good at shape and bad at numbers, so I had to check every count it produced by hand.

This is the same lesson, applied to refactoring. Here the agent never counts anything; the generator does. The agent gets one rule with one violation and a picture that says pass or fail after every change. It is the same reason a failing test is a better instruction than a paragraph of requirements: the acceptance criterion exists before the work starts, and it is not the agent that judges it.

What I did not get is the color. CRAP and mutation scores come from Uncle Bob's Clojure tooling, and there is no Rust equivalent wired in yet. The README is clear that a box with no metrics is painted as the worst grade, so the fill on my boxes means nothing until coverage and mutation numbers exist for Rust. The arrows alone were worth the setup.

## Do it yourself

1. Install the Clojure CLI and Java 21 or newer, then clone [uml-viewer](https://github.com/unclebob/uml-viewer).
2. Write a generator for your language. One class per module, one dependency edge per import, output as EDN with `:hierarchical true`. The generated `examples/uml-viewer.edn` in the repo is a complete example of the format. In Rust, imports are `use` paths; in TypeScript they are `import` statements; in Python, `import` and `from`. A regex over each file is enough to start.
3. Write the `:levels` list by hand, inner layer first. Describe the architecture you want; the red arrows will show where the code differs.
4. Run the viewer against your file. On a fresh start it waits for its companion agent; the `--restart` flag restores the last view without spawning one, which is what you want when you drive it from your own agent session.
5. Pick one red arrow. Write one instruction that names the two modules and the new home. Regenerate. Repeat.

## Common questions

<details class="mk-faq">
<summary>What is uml-viewer?</summary>
<div class="mk-faq__body">

uml-viewer is an open-source desktop tool by Uncle Bob (Robert Martin) that draws a codebase as a UML-like diagram you can click. Namespaces are components, modules are their elements, and a dependency that points from an inner layer to an outer layer is drawn red. It is built to be used together with a coding agent: you look at the diagram, tell the agent what to change, and the diagram is redrawn.

</div>
</details>

<details class="mk-faq">
<summary>Does uml-viewer work with Rust?</summary>
<div class="mk-faq__body">

Not directly. The only parser it ships today reads Clojure. The diagram is drawn from a plain EDN file that lists modules, dependency edges and layer levels, so for Rust I wrote a small script that walks the source tree and emits that file. Any language works the same way if you can list its modules and their imports.

</div>
</details>

<details class="mk-faq">
<summary>What is the Dependency Rule?</summary>
<div class="mk-faq__body">

The Dependency Rule from Clean Architecture says that source code dependencies must point inward, toward the parts of the system that change least. Your domain types should not import your HTTP handlers. In uml-viewer you list which modules sit at which level, and every arrow that points the wrong way is drawn red.

</div>
</details>

<details class="mk-faq">
<summary>How do you find circular dependencies between Rust modules?</summary>
<div class="mk-faq__body">

The compiler will not tell you, because Rust allows two modules in one crate to import each other. You have to build the module graph yourself: one node per file, one edge per use path that starts with crate, super or self, then look for pairs that point at each other. In my codebase that turned up 35 such pairs, most of them between storage, quotas, api and web.

</div>
</details>

<details class="mk-faq">
<summary>Why not just ask the AI agent to refactor the codebase?</summary>
<div class="mk-faq__body">

Because that instruction has no test. The agent will move things and call it done, and you have no way to say whether the result is better. A diagram gives you one red arrow at a time, and after each change you can see whether that arrow is gone and whether a new one appeared. The agent does the moving and the picture does the judging.

</div>
</details>

> **Key takeaways**
>
> - uml-viewer reads a plain EDN file, not source code. A 200-line script gets any language in.
> - The levels list is the only opinion in the input, so write the architecture you want and let the red show the distance.
> - Give the agent one rule, one violation and one destination per instruction. The next diagram is the acceptance test.
> - Rust will not tell you two modules import each other. Build the graph and count the mutual pairs yourself.
> - Stop when every remaining red arrow has a name. Mine are all "handler reads app state", and that is fine.
> - Turn the result into an allow-list test, and regenerate the graph after every feature push, because that is when regressions arrive.

The six commits are public in the [Uptimepage repository](https://github.com/uptimepage/uptimepage/compare/b791c262...cee12c44) if you want to read what moved and why.

## Sources

- Robert C. Martin, [uml-viewer](https://github.com/unclebob/uml-viewer) on GitHub. The README documents the EDN format, the `:levels` rule and the companion agent mailbox.
- Robert C. Martin, [The Clean Architecture](https://blog.cleancoder.com/uncle-bob/2012/08/13/the-clean-architecture.html), The Clean Code Blog, August 2012. The Dependency Rule.
- Uptimepage, [source repository](https://github.com/uptimepage/uptimepage), AGPL. Commits `bda59342` through `cee12c44` are the six rounds described here.
