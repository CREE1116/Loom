Correct the independent functions below without changing their signatures. Make all fixes.

Fix clamp_score in src/limits.rs to clamp any i64 to [0, 100].
Fix slugify in src/names.rs: lowercase ASCII letters, keep only ASCII a-z and 0-9, replace every run of other characters with one hyphen, and trim hyphens at both ends.
Fix median_sorted in src/stats.rs: input is already sorted. Empty input returns None, odd lengths return the middle value, even lengths return the average of the middle two.
Implement parse_duration_ms in src/duration.rs: trim surrounding whitespace; accept unsigned integers followed by ms, s, or m (multiply by 1, 1000, or 60000 respectively); reject missing units, negatives, decimals, trailing junk, empty numeric parts, and overflow.
