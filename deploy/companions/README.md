# Companion services catalog

One file per service, `<name>.conf`. [`scripts/install.sh`](../../scripts/install.sh)
reads them to suggest companion services after installing Cuthulu (see
[docs/DEPLOYMENT.md](../../docs/DEPLOYMENT.md#companion-services)); other
tools may read them to map a container to its systemd unit and env file.

## Format

Plain `KEY=value` lines. Blank lines and lines starting with `#` are ignored.
The value is the rest of the line after the first `=`, verbatim: no quotes, no
escapes, no variable expansion, no trailing comments. Nothing is executed (the
files are parsed, never `source`d). Each key appears at most once; unknown keys
are ignored, so readers can add their own.

| Key           | Required | Meaning |
|---------------|----------|---------|
| `NAME`        | yes      | Service name; must equal the file name without `.conf`. Lowercase letters, digits, `.`, `_`, `-`. Also the directory it is cloned to. |
| `DESCRIPTION` | yes      | One short line, shown in the install prompt. |
| `REPO`        | yes      | Git URL to clone. The repository must have a `scripts/install.sh` that installs or upgrades the service, honours an exported `IMAGE`, and uses sudo itself when it needs root. |
| `IMAGE`       | yes      | Published Docker image. Without a tag, `:latest` is installed; an explicit tag or digest is kept. |
| `CONTAINER`   | yes      | Name of the container the service runs as. |
| `UNIT`        | yes      | systemd unit name (`.service` optional). |
| `UNIT_SCOPE`  | yes      | `system` (`systemctl`) or `user` (`systemctl --user`). |
| `ENV_FILE`    | yes      | The service's settings file. `~` (user scope) means the installing user's home. |
| `SUGGEST`     | no       | `true` (default) or `false`. `false` keeps an entry in the catalog but out of the install prompt; `cuthulu.conf` uses it, since install.sh installs Cuthulu itself. |

Detection (shown as the entry's status): `running` when the unit is active in
its scope or the container runs, `installed` when systemd knows the unit but it
is not active, else `not installed`.

## Adding a service

1. Add `<name>.conf` here with the keys above.
2. Check it: `scripts/install.sh --list-companions` lists it with its status
   (a malformed file is reported and skipped), and `scripts/test-companions.sh`
   still passes.
3. Install it without reinstalling Cuthulu:
   `scripts/install.sh --companions-only --companions <name>`.
