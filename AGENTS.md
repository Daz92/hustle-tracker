# AGENTS.md

Operating rules for AI agents and humans working in this **fork** of
`adolfousier/hustle-tracker`.

Read this before touching Docker, `compose.yml`, or git remotes. Several
non-obvious traps in this project cause silent data loss or hours of debugging a
bug that was already fixed.

---

## 1. Repository model

| Remote | URL | Purpose |
| --- | --- | --- |
| `origin` | `git@github.com:Daz92/hustle-tracker.git` | Our fork. Push here. |
| `upstream` | `https://github.com/adolfousier/hustle-tracker` | Read-only. Push URL is deliberately set to `DISABLED`. |

### Branches

- **`main`** — a pure mirror of `upstream/main`. Fast-forward only.
- **`fix/*`** — changes intended for an upstream PR. Must contain **only** generic
  bugfixes. No machine-specific paths, no fork tooling.
- **`fork/integration`** — what we actually build and run. Upstream + our fixes +
  fork-only tooling (`just sync-upstream`, this file).

### MUST

- Keep `main` a pure mirror. Run `just sync-upstream` to update it.
- Put changes on a topic branch.
- **Rebase** onto `upstream/main`, don't merge. Patches stay a clean stack and
  vanish automatically once upstream lands an equivalent fix.
- Keep upstreamable bugfixes separate from fork-only tooling, in different commits
  on different branches.

### MUST NOT

- **Never commit to `main`.** It breaks `--ff-only` and every future sync.
- **Never push to `upstream`.** The push URL is disabled on purpose; do not re-enable it.
- Never put fork tooling or local paths in a `fix/*` branch destined for a PR.

---

## 2. Docker and the database — destructive command list

Tracked time is stored **only** in the Docker volume `hustle-tracker_postgres_data`.
There is no other copy. There is no automatic backup.

### MUST NOT — these destroy all tracked history

| Command | What it does |
| --- | --- |
| `docker compose down -v` | `-v` deletes the volume. **All tracked time is gone.** |
| `just clean` | Runs `cargo clean` **and `docker compose down -v`**. The name is misleading — it is not a build-only clean. |
| `just uninstall` | Deletes the volume *and* `rm -rf`s the repo directory. |
| `docker volume rm hustle-tracker_postgres_data` | Direct deletion. |

To apply config changes, recreate **without** `-v`:

```bash
docker compose up -d --force-recreate   # keeps the volume
```

Before any destructive operation, verify what's actually in the volume rather than
assuming it's empty:

```bash
docker run --rm -v hustle-tracker_postgres_data:/v alpine \
  sh -c 'find /v -maxdepth 3 -name PG_VERSION -exec cat {} \; ; du -sh /v'
```

To back up first:

```bash
docker exec hustle-tracker-postgres-1 \
  pg_dump -U "$POSTGRES_USERNAME" -d hustle-tracker > backup.sql
```

---

## 3. `compose.yml` invariants

Two regressions here already cost real debugging time. Preserve both.

### Volume mount — Postgres 18+

```yaml
volumes:
  - postgres_data:/var/lib/postgresql        # CORRECT
# - postgres_data:/var/lib/postgresql/data   # WRONG - container exits 1
```

Postgres 18 moved `PGDATA` to `/var/lib/postgresql/18/docker`. The entrypoint
hard-rejects a volume at `/var/lib/postgresql/data` and exits 1 before `initdb`
runs. Docker then silently creates an *anonymous* volume for
`/var/lib/postgresql`, so data would not persist even if it did start.

**MUST NOT** "restore" the `/data` suffix to match older docs or upstream.

### Healthcheck — escape the interpolation

```yaml
test: ["CMD-SHELL", "pg_isready -U \"$${POSTGRES_USER:-postgres}\" -d \"$${POSTGRES_DB:-hustle-tracker}\""]
```

`$$` escapes **compose** interpolation so the shell *inside the container* expands
the variables from its own environment.

**MUST NOT** use `${POSTGRES_USERNAME}` here. Compose interpolates on the host at
parse time; any invocation without `.env` loaded bakes in an empty value:

```
pg_isready -U  -d hustle-tracker
```

`-U` then consumes nothing, `hustle-tracker` is parsed as a stray positional
argument, and every probe fails with `too many command-line arguments` (exit 3).
The database works fine while the container reports `unhealthy` forever.

### Restart policy

Keep `restart: unless-stopped` so Postgres survives a reboot instead of relying on
`DockerManager`'s slow recovery path at every login.

---

## 4. There are TWO compose files — do not confuse them

| Path | Role |
| --- | --- |
| `<repo>/compose.yml` | Source of truth. Edit this one. |
| `~/.local/share/hustle-tracker/compose.yml` | **Runtime copy.** What the app actually launches. |

`DockerManager::ensure_compose_file` (`src/config/docker.rs:50`) writes the runtime
copy **only when it is absent**. It never refreshes a stale one. `COMPOSE_YML` is
embedded via `include_str!`, so *even a full rebuild will not update an existing
install.*

### MUST

- After editing `compose.yml`, run **`just sync-runtime-compose`** (or
  `just sync-upstream`, which calls it).
- When debugging container behaviour, check **which** file created the container:

```bash
docker inspect hustle-tracker-postgres-1 \
  --format '{{index .Config.Labels "com.docker.compose.project.config_files"}}'
```

If your repo fix appears to have no effect, this is almost always why.

---

## 5. Credentials and `.env`

`Settings::env_dir()` (`src/config/settings.rs:29`) prefers `./.env` if it exists,
otherwise falls back to the data dir. Credentials are auto-generated on first run.

- Canonical file: `~/.local/share/hustle-tracker/.env`
- Repo `.env` is a **symlink** to it, so repo-root compose invocations
  (`just db-up`, `just run`, `just dev`) resolve real credentials.
- `.env` is gitignored (`.gitignore:28`). Keep it that way.

### MUST NOT

- Never commit `.env` or paste credentials into commits, issues, or PRs.
- Never delete the repo `.env` symlink without understanding that `just db-up` will
  then silently create a container with a blank `POSTGRES_USER`.
- Never hand-edit credentials in only one place — `POSTGRES_USERNAME`,
  `POSTGRES_PASSWORD`, and `DATABASE_URL` must stay consistent.

Database facts: name `hustle-tracker`, host port **52851**, container port 5432.

---

## 6. Daemon lifecycle

- Binaries: `target/release/hustle_tracker` (TUI), `target/release/hustle_daemon`.
- `just daemon-start` / `daemon-stop` / `daemon-status` use `daemon.pid` in the repo root.
- `daemon-start` depends on `db-up` **and** `build-daemon`, so it runs a `cargo build`
  and a compose up. Not suitable for autostart.

### MUST

- Guard against duplicate daemons — the binary has no internal singleton lock; the
  only protection is the PID file and a `pgrep` check.
- Keep the PID file accurate. Write `$$` *before* `exec` so the PID survives.

---

## 7. Machine-specific integration lives OUTSIDE the repo

This is why the fork delta stays small. Do not move any of it into the repo.

| Path | Purpose |
| --- | --- |
| `~/.local/bin/hustle-daemon-autostart` | Autostart wrapper with duplicate guard |
| `~/.local/bin/hustle-tracker` | Symlink to the TUI release binary |
| `~/.local/share/applications/hustle-tracker.desktop` | Walker / omarchy menu entry |
| `~/.config/hypr/autostart.conf` | `exec-once` line for the daemon |

The daemon runs from Hyprland's `autostart.conf` rather than a plain systemd unit
because window tracking needs `HYPRLAND_INSTANCE_SIGNATURE` from the live session.

### MUST NOT

- Never edit anything under `~/.local/share/omarchy/` — it is git-managed by Omarchy
  and changes are lost on update. Reading it is fine.
- Never add personal paths (`/home/daz/...`) to tracked repo files.

---

## 8. Verifying a change

Don't declare a Docker fix working based on `docker ps` alone — a container can be
`Up` and still `unhealthy`, and it can be `healthy` while pointed at the wrong config.

```bash
# health, with actual probe output on failure
docker inspect hustle-tracker-postgres-1 --format '{{json .State.Health}}'

# the credentials the container really got
docker inspect hustle-tracker-postgres-1 \
  --format '{{range .Config.Env}}{{println .}}{{end}}' | grep '^POSTGRES_'

# data intact
docker exec hustle-tracker-postgres-1 \
  psql -U "$POSTGRES_USERNAME" -d hustle-tracker -tAc \
  "select count(*) from information_schema.tables where table_schema='public'"

# daemon actually writing (not merely alive)
docker exec hustle-tracker-postgres-1 \
  psql -U "$POSTGRES_USERNAME" -d hustle-tracker -tAc 'select max(start_time) from sessions'
```

After Hyprland config changes: `hyprctl reload && hyprctl configerrors`.

---

## 9. Syncing with upstream

```bash
just sync-upstream    # fetch, ff main, rebase topic branch, sync runtime compose
```

Then rebuild, because binaries do not track source automatically:

```bash
cargo build --release --bin hustle_tracker --bin hustle_daemon
just daemon-stop && just daemon-start
```

`compose.yml` is the known conflict hotspot — it is the one file both we and
upstream edit. On conflict, re-read section 3 and keep **both** invariants; do not
blindly accept upstream's version.

Upstream context: last commit 2026-03-16, dormant since. `upstream/main` still ships
both bugs from section 3, so a fresh upstream clone is currently broken on
Postgres 18.
