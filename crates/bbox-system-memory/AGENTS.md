# bbox-system-memory — loaded runbook catalog

## Procedures are skills, not memories

- A multi-step procedure lives in exactly one place, an agent skill under
  `system-defaults/skills/` or `.agents/skills/`. A system memory holds a
  retirement note or cold reference. A memory that restates a skill is a
  second copy to keep in step, and the two drift.
- Tests that need a real default memory use one that is not a procedure
  (`sm-create-etiquette`, `sm-atoms`), so retiring a procedure into a skill
  does not move them.

## Signposts vs bodies

- Broad recall surfaces system memories as signposts, not runbook bodies.
  `format_for_signpost` must stay compact: id/ref, bounded tags, one-line
  preview, and an exact-id breadcrumb. Full bodies are for exact
  `bbox_knowledge(query="sm-*")` retrieval; dumping them into fuzzy recall
  recreates the bbox_knowledge spill class.
