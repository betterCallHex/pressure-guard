#!/bin/zsh
set -euo pipefail

label=com.pressure-guard
plist=~/Library/LaunchAgents/$label.plist
binary=~/.cargo/bin/pressure-guard

cargo install --path "${0:A:h}" --locked --quiet

mkdir -p ~/Library/LaunchAgents ~/Library/Logs
cat > $plist <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key><string>$label</string>
    <key>ProgramArguments</key><array><string>$binary</string></array>
    <key>RunAtLoad</key><true/>
    <key>KeepAlive</key><true/>
    <key>ProcessType</key><string>Interactive</string>
    <key>StandardOutPath</key><string>$HOME/Library/Logs/pressure-guard.log</string>
    <key>StandardErrorPath</key><string>$HOME/Library/Logs/pressure-guard.log</string>
</dict>
</plist>
EOF

launchctl bootout gui/$(id -u)/$label 2>/dev/null || true
launchctl bootstrap gui/$(id -u) $plist
echo "pressure-guard is running. Log: ~/Library/Logs/pressure-guard.log"
