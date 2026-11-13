---
name: forest
description: "Use the forest command inside a bento box whenever the repos around you matter: what this forest checks out (forest ls), adding a repo, naming the forest's work, handing work to a new forest (forest new --brief), or running a GPU/display job on the host (forest gpu run). forest handles forests; hc handles agents."
---

# forest: the repos you share

A **forest** is a unit of work: a set of repos checked out together in one
bento box, with any number of agents working in it. You are in one now: each
`/workspace/<repo>` is one of its checkouts, all on the forest's branch.

```sh
forest ls    # this forest (name, branch, checkouts) + repos available on the host
```

**`forest` handles forests, `hc` handles agents.** Starting, reading, steering
or waiting for agents — including your own workers — is hc's job: run
`hc --skill` and follow it before starting any agent. If `$BENTO_BRIDGE_TOKEN`
is unset you are not in a box, and this skill does not apply.

## Name the work

A forest often starts nameless, as `wip-…`. Once the work is clear — when the
plan is written, before workers start — give it a name:

```sh
forest name fix-auth-flow    # renames the branch of every checkout
```

Only the branch changes; the folder keeps its old name. Say the name in
reports. Naming later, with workers running, confuses them: name first.

## Add a repo

```sh
forest add ois               # /workspace/ois appears after a restart
```

Adding a repo needs a box restart, which stops every agent in this box — you
included. Add repos **before** starting workers. If workers (or you) must keep
running, do not restart: tell your user or parent agent and let them do it.

## Several agents in this forest

Parallel work stays in this forest and uses hc, not forest:

```sh
hc run --bg --name impl opencode "Implement step 2 of the plan. Edit only src/auth/."
hc run --bg --name review opencode "Review src/auth/ against the plan. Read-only."
hc wait --children all
```

Give each worker disjoint files and an exact scope; they report back through
hc's parent/child notes (`hc --skill`). One agent taking over is the same,
with a single worker.

## Work in another forest: handoff

Work that needs other repos, or its own box with its own agents, is a handoff:
a new forest plus a first agent with a brief.

```sh
forest new port-ois ois --from main --brief brief.md
```

`--from` is the base branch (a repo's default if omitted); `--brief -` reads
the brief from stdin. The command creates the forest, its box and the first
agent, and prints the agent's hc terminal address. That agent cannot report
back to you, and cannot read your conversation: the brief is its whole
context. A good brief has:

- the goal, in a sentence or two;
- context: which repos and files (as the new agent sees them), where plans and
  notes are;
- constraints: what not to touch, hard rules;
- how to verify the work is done;
- where to write its notes, and who asked (it can only name you, not reach you).

## GPU jobs

The box has no GPU. Queue a job that runs on the host, in a jail: GPU and
display available, no network, only this forest's files writable.

```sh
forest gpu run --cwd game --timeout 60 -- './run.sh & sleep 10; grim "$GPU_JOB_FILES/start.png"; kill %1'
```

- One argument after `--` is a shell snippet (as above); several arguments are
  one command, no snippet (`-- cargo test`).
- The job's cwd is where you are, or `--cwd <repo>/sub`; it must be inside
  `/workspace/<checkout>`.
- `forest gpu run` waits, prints the job's stdout/stderr and exits with its
  exit code. `--no-wait` just queues; then `forest gpu status <id>` and
  `forest gpu ls`.
- Results land in `<checkout>/.gpu-jobs/<id>/`: `stdout`, `stderr`, `exit`
  (written last: it existing means the job is done), and `files/` — the job's
  `$GPU_JOB_FILES`, where it writes screenshots and other artifacts;
  `$GPU_JOB_DIR` is the whole result dir.
- One GPU job at a time on the whole machine, across all forests: keep jobs
  short. Timeout defaults to 600 s, max 3600 s; a timed-out job is killed and
  marked.
- A job cannot commit (git is hidden in the jail): move results out of
  `$GPU_JOB_FILES` yourself, then commit.

## What you cannot do

- Push, fetch or pull: the user does all remote git traffic. Committing is fine.
- See the host, or any other forest: only this forest's checkouts.
- Use the GPU or its display directly: only through `forest gpu run`.

## Errors

- exit 3: the bridge is down — the host is not reachable. Ask the user to run
  `bento serve` on the host.
- exit 2: refused — the host said no (name taken, not allowed); stderr says why.
- exit 1: anything else; stderr has the message.
