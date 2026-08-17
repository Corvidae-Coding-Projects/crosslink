


Output ../assets/img/quickstart.gif

Require crosslink

Set Shell "bash"
Set FontSize 14
Set Width 950
Set Height 550
Set Padding 20
Set Theme { "name": "crosslink", "black": "#1a1a2e", "red": "#f95838", "green": "#007c35", "yellow": "#f0a500", "blue": "#00a6db", "magenta": "#c77dff", "cyan": "#56cfe1", "white": "#e0e0e0", "brightBlack": "#555555", "brightRed": "#ff6b6b", "brightGreen": "#51cf66", "brightYellow": "#ffd43b", "brightBlue": "#74c0fc", "brightMagenta": "#d0a9f5", "brightCyan": "#99e9f2", "brightWhite": "#ffffff", "background": "#16161e", "foreground": "#e0e0e0", "selection": "#33467c", "cursor": "#f95838" }
Set TypingSpeed 35ms
Set WindowBar Colorful
Set WindowBarSize 40


Type "# Step 1: Install crosslink"
Enter
Type "cargo install crosslink"
Enter
Sleep 2s


Type "# Step 2: Initialize in your project"
Enter
Type "crosslink init"
Enter
Sleep 2s


Type "# Step 3: Agent starts a session (reads previous handoff)"
Enter
Type "crosslink session start"
Enter
Sleep 2.5s


Type "# Step 4: Agent creates an issue and begins work"
Enter
Type `crosslink quick "Fix login bug" -p high -l bug`
Enter
Sleep 1.5s


Type "# Step 5: Agent records breadcrumbs as it works"
Enter
Type `crosslink session action "Found root cause in refresh_token()"`
Enter
Sleep 1s
Type `crosslink issue comment 1 "Token wasn't refreshed on 401 responses"`
Enter
Sleep 1.5s


Type "# Step 6: Agent saves handoff for the next session"
Enter
Type `crosslink session end --notes "Fixed auth bug #1. Dark mode (#2) is next."`
Enter
Sleep 3s
