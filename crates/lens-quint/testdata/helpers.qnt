module helpers {
  pure def inc(n: int): int = n + 1
}

module extra {
  import helpers.*
  pure def twice(n: int): int = inc(inc(n))
}
