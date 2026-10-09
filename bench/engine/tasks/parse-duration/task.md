`parse_duration` must accept strings like "1h30m", "45m", "2h" and "90s" and return the total number of seconds. Currently minutes and seconds are ignored. Fix it; return None for invalid input.
