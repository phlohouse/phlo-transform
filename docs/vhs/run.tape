# Seed DuckDB sources and run the translated workspace end to end.
Output docs/assets/run.gif

Set FontSize 15
Set Width 1100
Set Height 640
Set Padding 16
Set TypingSpeed 40ms
Set Theme "Catppuccin Mocha"

# Requires `phlo-transform` on PATH (e.g. cargo install --path crates/phlo-transform-cli)
Env SHELL "/bin/zsh"

Type "cd /tmp/phlo-demo/generated"
Enter
Sleep 500ms

Type "# create the dbt sources as ordinary DuckDB tables"
Enter
Sleep 300ms
Type "duckdb shop.duckdb < ../seed.sql"
Enter
Sleep 1.5s

Type "phlo-transform doctor --adapter duckdb --duckdb-path shop.duckdb"
Enter
Sleep 2s

Type "phlo-transform run --adapter duckdb --duckdb-path shop.duckdb"
Enter
Sleep 6s
