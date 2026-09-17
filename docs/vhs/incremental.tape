# A second run picks up new source rows through the incremental strategies.
Output docs/assets/incremental.gif

Set FontSize 15
Set Width 1100
Set Height 640
Set Padding 16
Set TypingSpeed 40ms
Set Theme "Catppuccin Mocha"

# Requires `phlo-transform` on PATH (e.g. cargo install --path crates/phlo-transform-cli)
Env PATH "$HOME/.cargo/bin:/usr/local/bin:/usr/bin:/bin:/opt/homebrew/bin"
Env SHELL "/bin/zsh"

Type "cd /tmp/phlo-demo/generated"
Enter
Sleep 500ms

Type "# a new order lands, and an existing one is updated"
Enter
Sleep 300ms
Type "duckdb shop.duckdb < ../mutate.sql"
Enter
Sleep 1.5s

Type "phlo-transform run --adapter duckdb --duckdb-path shop.duckdb"
Enter
Sleep 6s

Type "duckdb shop.duckdb -c 'select * from marts.orders_incremental order by order_id'"
Enter
Sleep 2.5s
