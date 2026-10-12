---
name: bbox-persist
description: Propose durable Blackbox knowledge and save only operator-approved exact text and scope. Use when the user states a standing rule, convention, preference or ban ("from now on", "always", "never", "we don't use", "remember this"), asks to change or forget one, after you implement a directive that should outlive this session, and before any bbox_learn, bbox_forget or bbox_render call.
---

# Proposing and persisting durable knowledge

`bbox_learn` writes knowledge that later sessions load as instructions. A wrong or over-broad entry misdirects every future agent, and nothing reviews an entry after the write. So the operator approves the exact text, lane and scope first. Proposing is your job; deciding is theirs. A request to do today's task is not approval of a future rule.

Enforcing a rule in code or config handles today's edit. It does not tell the next session the rule exists. After implementing any user directive, ask whether the user just stated a standing rule. If so, propose persisting it before you reply.

## 1. Pick the lane

Stop at the first yes.

1. **Is it state for one line of work** (a debugging walk, a migration phase, sequencing for an active initiative, a charter for one executor)? It belongs in the dispatch brief or a `bbox_thread` note. It is not knowledge, however much you want it to stay visible.
2. **Did the user state it as a standing rule, and would it still be correct a year from now with every current initiative finished?** Propose a rendered entry.
3. **Is it a useful fact worth finding later but not worth loading into every session?** Propose a recall entry (`render=false`).

Constraints for the current task only ("skip the tests for this fix", "just for today") are not persisted at all.

Who said it matters. A rule the user stated leans toward a rendered entry. Something you discovered while working is not policy, even when it is durable: it goes in a thread note or, at most, a `render=false` entry. Do not bury a user directive in a thread note either, because later sessions will not load it from there. Do not infer operator intent from your own findings or from deleted material.

When unsure between rendered and recall, choose `render=false`.

## 2. Pick the scope

Send `scope` explicitly on every `bbox_learn` call. The tool's default is global, so an omitted scope silently writes a rule for every project.

- `scope="project"` with `project=<repo>` for conventions of this repository. Choose this when both readings are plausible.
- `scope="global"` only when the statement reaches beyond this repository: "across every project", "on every machine", "in all the Rust I write", a personal rule. Strong wording alone is not global: "we always use tokio here" is project scope.

## 3. Check for an existing entry

```text
bbox_knowledge(query="<the rule's subject>", project="<repo>")
```

Read any likely match in full, including its scope. Look for the same topic under a different title, the same rule in the other scope, and an older entry this one should replace. Use the returned id to update, instead of creating a competing rule. Unpublished project edits may exist only in the owning checkout, so check there too.

## 4. Draft, propose, wait

Draft the exact `content`, the `category`, the scope and the rendered-or-recall choice. State the rule and its reason. Leave out task history, dates, names, issue numbers, temporary sequencing and private client details: the entry describes how things are, not how they came to be. Use `category="convention"` for a standing convention and `category="memory", render=false` for a cold fact.

Show the operator that proposal, with any entry it replaces or retires, and wait for approval of the exact text and scope. If the operator has already approved this precise write, proceed without asking again. General enthusiasm, an approval of similar wording, or elapsed time is not approval.

## 5. Save

```text
bbox_learn(content="Use rustls, not openssl: one TLS stack keeps builds reproducible.", category="convention", scope="project", project="/repo/x")
```

Pass `id` for an approved update. Omit `project` for a global write. `bbox_forget(id=..., project=...)` deletes, so it needs the same approval, for that exact entry.

## 6. Follow the write to where it lands

Read the receipt and confirm the stored result. A project entry is a file under `.bbox/knowledge/` in the owning checkout; where another host owns that checkout, the write is queued and a collector delivers it. Commit the intended file by explicit path when you are authorized to commit, and verify publication. Admission alone does not make the rule visible to future sessions. If the write cannot be completed, report the stage it reached.

Read [render-lifecycle.md](render-lifecycle.md) when a rendered entry must reach provider instruction files, when a rendered file looks stale, or when a render call is refused. Do not hand-edit generated instruction files: a managed region is regenerated from knowledge, so an edit there is lost on the next render. Correct the source entry instead.

Report the entry id, its scope, and whether storage, publication and rendering have each completed.
