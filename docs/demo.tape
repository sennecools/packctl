# packctl demo recording (https://github.com/charmbracelet/vhs).
#
# Rendered by .github/workflows/demo.yml, or locally from the repository root
# with packctl on PATH:
#
#   vhs docs/demo.tape
#
# Everything runs offline against the fixture built by scripts/demo/setup.sh.

Output docs/media/demo.gif

Require packctl

Set Shell "bash"
Set FontSize 16
Set Width 1200
Set Height 760
Set Padding 20
Set Theme "Catppuccin Mocha"
Set TypingSpeed 40ms

Hide
Type "export PS1='$ ' && . scripts/demo/setup.sh >/dev/null && clear"
Enter
Sleep 2s
Show

Type "packctl status"
Sleep 400ms
Enter
Sleep 3s

Hide
Type "clear"
Enter
Show

Type "packctl adopt --as demo-pack-1.0 --apply -y"
Sleep 400ms
Enter
Sleep 6s

Hide
Type "clear"
Enter
Show

Type "packctl plan --to demo-pack-1.1"
Sleep 400ms
Enter
Sleep 4s

Hide
Type "clear"
Enter
Show

Type "packctl update -n --to demo-pack-1.1"
Sleep 400ms
Enter
Sleep 5s

Hide
Type "clear"
Enter
Show

Type "packctl status"
Sleep 400ms
Enter
Sleep 3s

Type "packctl rollback"
Sleep 400ms
Enter
Sleep 4s
