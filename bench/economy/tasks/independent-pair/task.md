Correct the independent functions below without changing their signatures. Make all fixes.

Fix clamp_score in src/limits.rs to clamp any i64 to [0, 100].
Fix slugify in src/names.rs: lowercase ASCII letters, keep only ASCII a-z and 0-9, replace every run of other characters with one hyphen, and trim hyphens at both ends.
