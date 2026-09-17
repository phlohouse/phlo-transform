# Analyse a dbt project and emit a native Phlo workspace.
Output docs/assets/translate.gif

Set FontSize 15
Set Width 1100
Set Height 640
Set Padding 16
Set TypingSpeed 40ms
Set Theme "Catppuccin Mocha"

# Requires `phlo-transform` on PATH (e.g. cargo install --path crates/phlo-transform-cli)
Env PATH "$HOME/.cargo/bin:/usr/local/bin:/usr/bin:/bin:/opt/homebrew/bin"
Env SHELL "/bin/zsh"

Type "cd /tmp/phlo-demo/dbt-shop"
Enter
Sleep 500ms

Type "# 1. analyse only — writes nothing"
Enter
Sleep 300ms
Type "phlo-transform translate --from dbt --check"
Enter
Sleep 2.5s

Type "# 2. emit the translated workspace and compile-check it"
Enter
Sleep 300ms
Type "phlo-transform translate --from dbt --out ../generated --verify"
Enter
Sleep 3s

Type "# 3. what was generated"
Enter
Sleep 300ms
Type "find ../generated -type f | sort"
Enter
Sleep 2s
