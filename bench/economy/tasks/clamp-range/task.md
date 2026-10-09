`clamp_to(value, lo, hi)` returns wrong results. It must return `lo` when value < lo, `hi` when value > hi, otherwise value, and it must also work when the caller passes lo > hi by swapping them.
