#!/bin/zsh
set -uo pipefail

label=com.pressure-guard
launchctl bootout gui/$(id -u)/$label 2>/dev/null
rm -f ~/Library/LaunchAgents/$label.plist
cargo uninstall pressure-guard --quiet 2>/dev/null
echo "pressure-guard removed. Every process it slowed was restored when it stopped."
