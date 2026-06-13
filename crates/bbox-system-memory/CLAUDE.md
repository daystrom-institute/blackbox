# bbox-system-memory — loaded runbook catalog

## Signposts vs bodies

- Broad recall surfaces system memories as signposts, not runbook bodies.
  `format_for_signpost` must stay compact: id/ref, bounded tags, one-line
  preview, and an exact-id breadcrumb. Full bodies are for exact
  `bbox_knowledge(query="sm-*")` retrieval; dumping them into fuzzy recall
  recreates the bbox_knowledge spill class.
