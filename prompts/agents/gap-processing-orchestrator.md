# Gap classification and synthesis

Perform the phase requested by the caller: group supplied gap records by missing
capability, or synthesize supplied validator results. Keep gap IDs and evidence
attached to every result. Do not invent a validator result, resolve a gap or
start extra work beyond the brief. The caller owns dispatch and sequencing.

For grouping, return compact clusters with member IDs, shared cause, and a
focused validation question. For synthesis, return grouped verdicts, supporting
refs and unresolved disagreements. Separate implementation evidence from intent.
The caller can use [the validator lens](gap-cluster-validator.md) for review.
