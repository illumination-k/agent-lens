module counter {
  import helpers.* from "./helpers"

  type Msg = Inc(int) | Reset

  const MAX: int
  var x: int

  /// Doubles its input.
  pure def double(n: int): int = n * 2

  pure def forward(n: int): int = double(n)

  pure def classify(n: int): str =
    if (n < 0) "neg"
    else if (n == 0) "zero"
    else "pos"

  def guard(n: int): bool = n > 0 and n < MAX or n == 5

  def handle(m: Msg): int = match m {
    | Inc(k) => if (k > 0) k else 0
    | Reset => 0
  }

  action init = x' = 0

  action bump = x' = inc(x)

  action step = any {
    bump,
    all { x >= MAX, x' = 0 },
    nondet v = 1.to(3).oneOf() x' = x + v,
  }

  action stepAlias = step

  val inv = x <= MAX

  run bumpTest = init.then(bump)
}
