# Local Sync and Sandboxes

Cloud sync is not the only place usage can go. Two more providers keep everything on your own machine:

- **`fs`** syncs into a directory.
- **`http`** syncs through a hub, a small server on your machine that owns a directory and lets other
  processes (a container, a VM, a Docker sandbox) write to it.

The data layout, the merge rules and the dashboard are the same as for [cloud sync](/guide/cloud-sync). Each
process that syncs is a "machine" in the totals, so a hub with several sandboxes behind it gives you one
aggregate view and a per-sandbox view.

## Sync into a directory

```bash
ccusage sync setup --provider fs --dir ~/.ccusage-sync
ccusage sync run
ccusage sync dashboard --open
```

Every object is one plain file under the directory, so you can read it with `cat` and back it up with `cp`.
Several processes can use the same directory at once: writes take an advisory lock and land through a rename,
so a reader never sees half an object and two writers never overwrite each other.

## Serve a directory to other processes

On the machine that owns the data:

```bash
ccusage sync serve --dir ~/.ccusage-sync
```

The first run creates a random bearer token at `~/.config/ccusage/hub-token` (owner-only) and listens on
`127.0.0.1:8777`. Use `--listen`, `--token-file` and `--prefix` to change those. Binding a non-loopback address
works but prints a warning, because the token is then the only protection.

On each client, point setup at the hub and give it the token through the environment:

```bash
export CCUSAGE_SYNC_TOKEN="$(cat ~/.config/ccusage/hub-token)"
ccusage sync setup --provider http --url http://host.docker.internal:8777 \
  --machine-id my-sandbox --machine-label "My sandbox" --non-interactive
ccusage sync run
```

`--machine-id` pins the identity instead of minting one, which is what you want for a sandbox whose own id is
known. Machines that set up at the same moment all join one user: the first to create the manifest wins and the
others adopt its user id.

Read the result on the host with the same directory:

```json
{ "sync": { "provider": "fs", "dir": "~/.ccusage-sync" } }
```

```bash
ccusage sync dashboard --open
```

## See who has synced

```bash
ccusage sync machines
```

```text
MACHINE                      STATUS   LAST SYNC    DAYS  USAGE                     AGENTS
web-app-sandbox (98029dfe)   active   today        12    2026-09-30 to 2026-10-10  claude
data-pipeline (4243722f)     active   2 days ago   4     2026-10-02 to 2026-10-08  claude, codex
old-demo (687f69d9)          retired  30 days ago  3     2026-08-05 to 2026-10-09  claude
```

Machines are named by their `machineLabel`, followed by the start of their id. A sandbox that is removed never
says goodbye, so a machine that has not synced for 14 days is called retired. Change the window with
`--retired-after <days>`. Nothing is changed or deleted: a retired machine's usage stays in the totals, and the
label only helps you tell it apart. `--json` prints the same list for scripts.

The local dashboard uses the same information. Its Machines panel names each machine, shows whether it is active
or retired and when it last synced, and lists machines that have no usage of their own to count, such as one
whose usage was all reported by another machine too.

A sandbox that is removed and created again with the same name is a new machine, with a new id. The old one stays
on the list until it retires.

## What the hub protects against

Treat everything the hub receives as untrusted, because the process on the other end may be an AI agent.

- **Bearer token on every call.** It is compared in constant time. Without it nothing is read or written.
- **Key validation.** Keys are checked segment by segment and must sit under the hub's prefix, so no request can
  name a file outside the data directory.
- **Read-only dashboard prefix.** The prefix that is world-readable in a bucket cannot be written through the hub.
- **Limits.** Request bodies are capped at 32 MiB and connections at 32, and sockets time out after 30 seconds.
- **Deletes refused.** Clients cannot delete anything, apart from the doctor's probe object, unless you start the
  hub with `--allow-delete`.

One thing the hub does not do is tell clients apart: a single token serves them all, and the rollup objects are
shared. A misbehaving client can therefore write wrong numbers into the aggregate, but never read outside the
prefix or delete history. `ccusage sync repair` rebuilds the rollups from each machine's shards.

## Contention

Every machine updates the shared rollups with compare-and-swap. When many machines sync at the same instant,
some lose all five attempts and report that the rollups "kept changing". Nothing is lost: the shards are already
uploaded and the next run folds them in. A sync loop should wait a few seconds and run again.

## Settings and checks

`dir` and `url` live in the `sync` block next to `provider`; see [Configuration Files](/guide/config-files#cloud-sync-settings).
`ccusage sync status` shows the directory or hub in use, and `ccusage sync doctor` checks that the store can be
written, that it honors compare-and-swap, and, for a hub, that it is reachable and serves the expected prefix.

Commands that only make sense for a Google Cloud bucket (`sync share`, `sync remove`, `sync dashboard --deploy`)
say so instead of running.
