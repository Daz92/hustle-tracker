set windows-shell := ["powershell.exe", "-NoProfile", "-Command"]

binary := if os() == "windows" { "target/release/hustle_tracker.exe" } else { "target/release/hustle_tracker" }
daemon_binary := if os() == "windows" { "target/release/hustle_daemon.exe" } else { "target/release/hustle_daemon" }
pid_file := "daemon.pid"

default: help

help:
    @echo "Hustle Tracker - Just Commands"
    @echo "======================================"
    @echo ""
    @echo "Quick Start (Linux - Unified Mode):"
    @echo "  just run           - Start DB + build + run app (ONE COMMAND!)"
    @echo "  just dev           - Start DB + run app in dev mode (faster builds)"
    @echo ""
    @echo "macOS/Windows (Daemon Mode - Recommended):"
    @echo "  just daemon-start  - Start background tracking daemon"
    @echo "  just daemon-stop   - Stop background tracking daemon"
    @echo "  just daemon-status - Check if daemon is running"
    @echo "  just view          - Open TUI to view stats (daemon must be running)"
    @echo ""
    @echo "Individual Steps:"
    @echo "  just db-up         - Start PostgreSQL in Docker"
    @echo "  just build         - Build TUI binary only"
    @echo "  just build-daemon  - Build daemon binary only"
    @echo "  just db-down       - Stop PostgreSQL"
    @echo "  just clean         - Clean all build artifacts and stop DB"
    @echo ""
    @echo "Fork Maintenance:"
    @echo "  just sync-upstream - Fetch upstream, ff main, rebase branch, sync runtime config"
    @echo "  just sync-runtime-compose - Copy compose.yml into the app data dir"
    @echo ""
    @echo "Fork Maintenance:"
    @echo "  just sync-upstream - Fetch upstream, ff main, rebase branch, sync runtime config"
    @echo "  just sync-runtime-compose - Copy repo compose.yml into the app data dir"
    @echo ""
    @echo "Cleanup & Removal:"
    @echo "  just uninstall     - Remove app, database volume, and local directory"
    @echo ""
    @echo "Note: Credentials are auto-generated on first run!"

run: check-wayland db-up build
    @echo "Starting Hustle Tracker..."
    #!/usr/bin/env bash
    ./{{ binary }}

dev: check-wayland db-up
    @echo "Starting in development mode..."
    cargo run

build:
    @echo "Building TUI release binary..."
    cargo build --release --bin hustle_tracker

build-daemon:
    @echo "Building daemon release binary..."
    cargo build --release --bin hustle_daemon

check-docker-compose:
    #!/usr/bin/env bash
    if docker compose version >/dev/null 2>&1; then
        exit 0
    fi
    echo "Docker compose plugin not found. Attempting to set it up..."
    # Check if standalone docker-compose v2 exists and symlink it as a plugin
    if command -v docker-compose >/dev/null 2>&1; then
        version=$(docker-compose version 2>/dev/null || true)
        if echo "$version" | grep -q "v2"; then
            plugin_dir="${DOCKER_CONFIG:-$HOME/.docker}/cli-plugins"
            mkdir -p "$plugin_dir"
            ln -sf "$(command -v docker-compose)" "$plugin_dir/docker-compose"
            echo "Symlinked docker-compose as docker compose plugin."
            if docker compose version >/dev/null 2>&1; then
                echo "✓ docker compose is now available!"
                exit 0
            fi
        fi
    fi
    echo ""
    echo "ERROR: 'docker compose' is not available."
    echo ""
    echo "Install it with one of:"
    echo "  sudo apt-get install docker-compose-plugin"
    echo "  # or see https://docs.docker.com/compose/install/"
    echo ""
    exit 1

db-up: check-docker-compose
    @echo "Starting PostgreSQL..."
    docker compose up -d
    @echo "Waiting for database to be ready..."
    #!/usr/bin/env bash
    sleep 5

db-down:
    @echo "Stopping PostgreSQL..."
    docker compose down

check-wayland:
    #!/usr/bin/env bash
    if [ "$(uname)" != "Darwin" ] && [ "$(uname)" != "MINGW64_NT-10.0" ]; then
        if [ "$XDG_SESSION_TYPE" = "wayland" ] || [ -n "$WAYLAND_DISPLAY" ] || [ -e "${XDG_RUNTIME_DIR:-/run/user/$(id -u)}/wayland-0" ]; then
            echo "Wayland session detected!"
            # Hyprland has built-in support via hyprctl — no extension needed
            if [ -n "$HYPRLAND_INSTANCE_SIGNATURE" ]; then
                echo "✓ Hyprland detected - using hyprctl for window tracking."
            else
                echo "Checking for Window Calls GNOME extension..."
                if ! gnome-extensions list 2>/dev/null | grep -q "window-calls" && \
                   ! ls -d ~/.local/share/gnome-shell/extensions/window-calls* >/dev/null 2>&1 && \
                   ! ls -d /usr/share/gnome-shell/extensions/window-calls* >/dev/null 2>&1; then
                    echo ""
                    echo "⚠️  WAYLAND SETUP REQUIRED ⚠️"
                    echo ""
                    echo "The 'Window Calls' GNOME extension is required for Wayland support."
                    echo ""
                    echo "Install it by visiting:"
                    echo "  https://extensions.gnome.org/extension/4724/window-calls/"
                    echo ""
                    echo "Or install Extension Manager:"
                    echo "  sudo apt install gnome-shell-extension-manager"
                    echo ""
                    echo "After installing, re-run 'just run'"
                    echo ""
                    exit 1
                else
                    echo "✓ Window Calls extension found!"
                fi
            fi
        else
            echo "X11 session detected - no additional setup needed."
        fi
    fi

clean:
    @echo "Cleaning build artifacts..."
    cargo clean
    @echo "Stopping and removing database..."
    docker compose down -v
    @echo "Clean complete!"

setup: db-up

daemon-start: db-up build-daemon
    #!/usr/bin/env bash
    echo "Starting background daemon..."
    if [ -f {{ pid_file }} ]; then
        echo "Daemon already running (PID: $(cat {{ pid_file }}))"
        echo "Run 'just daemon-stop' first to restart"
        exit 1
    fi
    if [ "$(uname)" = "Darwin" ] || [ "$(uname)" = "MINGW64_NT-10.0" ]; then
        nohup ./{{ daemon_binary }} > daemon.log 2>&1 & echo $! > {{ pid_file }}
        echo "Daemon started (PID: $(cat {{ pid_file }}))"
        echo "Logs: daemon.log"
        echo "To view stats: just view"
    else
        nohup ./{{ daemon_binary }} > daemon.log 2>&1 & echo $! > {{ pid_file }}
        echo "Daemon started (PID: $(cat {{ pid_file }}))"
        echo "Logs: daemon.log"
        echo "To view stats: just view"
    fi

daemon-stop:
    #!/usr/bin/env bash
    if [ ! -f {{ pid_file }} ]; then
        echo "Daemon not running (no PID file found)"
        exit 1
    fi
    echo "Stopping daemon (PID: $(cat {{ pid_file }}))..."
    kill $(cat {{ pid_file }}) 2>/dev/null || echo "Process already stopped"
    rm -f {{ pid_file }}
    echo "Daemon stopped"

daemon-status:
    #!/usr/bin/env bash
    if [ ! -f {{ pid_file }} ]; then
        echo "Daemon is NOT running"
        exit 1
    fi
    if ps -p $(cat {{ pid_file }}) > /dev/null 2>&1; then
        echo "Daemon is RUNNING (PID: $(cat {{ pid_file }}))"
        echo "Logs: daemon.log"
    else
        echo "Daemon is NOT running (stale PID file)"
        rm -f {{ pid_file }}
        exit 1
    fi

view: build
    #!/usr/bin/env bash
    if [ ! -f {{ pid_file }} ]; then
        echo "⚠️  Warning: Daemon not running"
        echo "Start daemon first: just daemon-start"
        echo ""
        echo "Opening TUI in viewer mode anyway..."
    fi
    echo "Opening TUI..."
    ./{{ binary }}

build-release-linux:
    @echo "Building release binaries for Linux..."
    ./src/scripts/build-binaries.sh ./dist linux

build-release-macos:
    @echo "Building release binaries for macOS..."
    ./src/scripts/build-binaries.sh ./dist macos

build-release-windows:
    @echo "Building release binaries for Windows..."
    @{{ if os() == "windows" { "powershell -Command \".\\src\\scripts\\build-binaries.ps1 -OutputDir ./dist -ReleaseTag windows\"" } else { "echo 'Run on Windows with PowerShell'" } }}

build-all-releases:
    @echo "Building all release binaries..."
    @echo "Run this on each platform:"
    @echo "  Linux:   just build-release-linux"
    @echo "  macOS:   just build-release-macos"
    @echo "  Windows: just build-release-windows"

# DockerManager::ensure_compose_file only writes the runtime compose.yml when it is
# ABSENT, so an existing install keeps a stale copy forever and upstream compose
# fixes never take effect - not even after a full rebuild.
# Copy compose.yml into the app data dir so the running config matches the repo
sync-runtime-compose:
    #!/usr/bin/env bash
    set -euo pipefail
    if [ "$(uname)" = "Darwin" ]; then
        data_dir="$HOME/Library/Application Support/hustle-tracker"
    else
        data_dir="${XDG_DATA_HOME:-$HOME/.local/share}/hustle-tracker"
    fi
    runtime="$data_dir/compose.yml"
    if [ ! -d "$data_dir" ]; then
        echo "No data dir yet ($data_dir) - nothing to sync."
        exit 0
    fi
    if [ -f "$runtime" ] && cmp -s compose.yml "$runtime"; then
        echo "✓ Runtime compose.yml already matches the repo."
        exit 0
    fi
    if [ -f "$runtime" ]; then
        cp "$runtime" "$runtime.bak.$(date +%s)"
        echo "Backed up previous runtime compose.yml"
    fi
    cp compose.yml "$runtime"
    echo "✓ Synced compose.yml -> $runtime"
    echo ""
    echo "⚠️  The running container still uses the OLD config. Apply it with:"
    echo "      docker compose up -d --force-recreate"
    echo "    (no -v: that would delete your tracked data)"

# Fetch upstream, fast-forward main, rebase the current topic branch, re-sync runtime config.
sync-upstream:
    #!/usr/bin/env bash
    set -euo pipefail

    if ! git remote get-url upstream >/dev/null 2>&1; then
        echo "No 'upstream' remote configured. Add it with:"
        echo "  git remote add upstream https://github.com/adolfousier/hustle-tracker"
        exit 1
    fi

    # A rebase over a dirty tree loses work. Refuse rather than stash implicitly.
    if [ -n "$(git status --porcelain --untracked-files=no)" ]; then
        echo "✗ Working tree has uncommitted changes. Commit or stash first:"
        git status --short
        exit 1
    fi

    branch="$(git rev-parse --abbrev-ref HEAD)"
    echo "Fetching upstream..."
    git fetch upstream

    behind="$(git rev-list --count HEAD..upstream/main)"
    if [ "$behind" = "0" ]; then
        echo "✓ Already up to date with upstream/main."
    else
        echo "upstream/main has $behind new commit(s):"
        git log --oneline --no-decorate "HEAD..upstream/main" | head -20
    fi

    echo ""
    echo "Fast-forwarding main..."
    git checkout main
    if ! git merge --ff-only upstream/main; then
        echo "✗ main has diverged from upstream/main."
        echo "  Keep main a pure mirror; put your changes on a topic branch."
        git checkout "$branch"
        exit 1
    fi

    if [ "$branch" = "main" ]; then
        echo "✓ On main; nothing to rebase."
    else
        echo ""
        echo "Rebasing $branch onto upstream/main..."
        git checkout "$branch"
        if ! git rebase upstream/main; then
            echo ""
            echo "✗ Rebase hit conflicts. compose.yml is the usual culprit."
            echo "  Resolve, then: git rebase --continue"
            echo "  Or bail out:   git rebase --abort"
            exit 1
        fi
    fi

    echo ""
    just sync-runtime-compose

    echo ""
    echo "Next: rebuild so the binaries match the new source"
    echo "  cargo build --release --bin hustle_tracker --bin hustle_daemon"
    echo "  just daemon-stop && just daemon-start"

uninstall:
    #!/usr/bin/env bash
    echo "Uninstalling Hustle Tracker..."
    rm -f {{ pid_file }}
    echo ""
    echo "This will:"
    echo "  1. Stop the PostgreSQL database"
    echo "  2. Remove the database volume (all tracked data)"
    echo "  3. Delete the local installation directory"
    echo ""
    read -p "Do you want to proceed? (yes/no): " response
    if [ "$response" = "yes" ]; then
        echo "Stopping Docker Compose..."
        docker compose down -v
        echo ""
        echo "⚠️  WARNING: This will delete the app directory from your computer!"
        read -p "Type 'yes' to confirm deletion of all files: " confirm
        if [ "$confirm" = "yes" ]; then
            echo "Removing installation directory..."
            cd ..
            rm -rf hustle-tracker
            echo "✓ Uninstall complete!"
        else
            echo "✗ Cancelled. Directory kept."
        fi
    else
        echo "✗ Uninstall cancelled."
    fi
