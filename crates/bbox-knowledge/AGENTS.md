# bbox-knowledge — durable knowledge store and recall renderer

## Recall output shape

- `Knowledge::list` owns durable-entry recall ranking and the default top-N
  presentation. A compact default is not a relevance filter: keep the full
  match count visible and make expansion explicit through `limit` or sharper
  queries. Changing scoring/matching to reduce output size compromises search
  integrity; output spill is handled by presentation bounds and excerpts.
