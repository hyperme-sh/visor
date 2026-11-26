# Local Visor terminal task

Visor can run one locally declared command at a time for the selected and
approved session on an assigned Unix workspace. The CLI and VS Code use the
same private companion connection. Use the existing permission view to inspect
the assignment and the local companion controls to approve or pause the session.

Create an owner-only `terminal-task.json` (mode `0600`) in the Visor config
directory (`$XDG_CONFIG_HOME/hypermesh`, `~/.config/hypermesh`, or the daemon's
`HYPERMESH_CONFIG_DIR`). Use the canonical assigned workspace path and an
existing canonical working directory inside it.

```json
{
  "workspace": "/home/developer/project",
  "cwd": "/home/developer/project",
  "command": "/bin/pwd",
  "args": [],
  "timeout_seconds": 30
}
```

The declaration is local owner configuration. Visor runs the executable
directly with the listed arguments and a fresh
environment containing only `PATH=/usr/bin:/bin` and `LANG=C`. The command
runs as the local Visor user with that user's normal OS permissions. Declare
only commands you trust to run with those permissions.

In VS Code, configure the installed companion path in User Settings and run
**Hypermesh: Visor terminal task** in a trusted local folder. The command shows
the current access and lets you review the workspace, executable, arguments,
working directory, environment policy, and timeout before starting. The
integrated terminal shows the task ID, output, and exit state. Use the same
menu to attach to a task by ID and optional cursor or to request cancellation.

From the CLI, inspect the declaration and run using its returned revision.

```sh
hypermesh-companion --terminal-task /home/developer/project context
hypermesh-companion --terminal-task /home/developer/project run REVISION
hypermesh-companion --terminal-task /home/developer/project watch TASK_ID 0
hypermesh-companion --terminal-task /home/developer/project cancel TASK_ID
```

Closing the terminal view detaches. Attach by the printed task ID to recover
output. A cursor is a byte offset starting at zero. Visor retains up to 128 KiB
of combined stdout and stderr per task, and up to 16 task IDs until their
sessions close or Visor restarts. The terminal view filters control characters.
A local pause stops active work. Resuming access requires a new explicit run.
Assignment loss or a change to the active declaration stops the command.

The native companion also offers a machine-readable `request` action on stdin.
The `context` operation returns the current session, assignment binding,
daemon instance, access and task revision. `start`, `read`, and `cancel` carry
that scope and the task ID. `start` also carries the reviewed revision.
`read` returns paged output and the next cursor. Reattach with `read`; a repeat
of the same start ID and revision on the same instance returns the existing run.

## Interactive input

Add `"interactive": true` to the owner-only declaration to give the command
a private pseudo-terminal. The default declaration keeps its output-only mode.
The interactive command gets `TERM=dumb` in addition to the clean environment.
It can prompt for input and react to terminal resizing. Visor stops all
process groups in the terminal session during teardown. The Linux path uses
kernel process handles to keep signalling bound to the original process.

In VS Code choose **Attach with input**, enter the task ID and optional
cursor, and confirm the claim or transfer. **Attach to task** follows output
as a viewer. Either view can stay open while input moves to another client.

From a local terminal, run:

```sh
hypermesh-companion --terminal-task /home/developer/project interact TASK_ID 0
hypermesh-companion --terminal-task /home/developer/project interact TASK_ID 0 --take-over
```

The input attachment forwards local terminal dimensions and keystrokes.
Ctrl+] detaches and prints the output cursor. Closing the VS Code terminal
also detaches. The task can continue while detached. Use **Cancel task**
or the CLI's `cancel` action to stop it.

Visor grants input to one attachment for the exact task and terminal. The
terminal view shows the current owner and last acknowledgement. Each input
or resize operation uses the current grant and a sequence number. Acknowledging
input means the terminal accepted the reported bytes. An interrupted client
reattaches by ID and cursor. The clients discard unsent keystrokes when an
attachment loses input access. Visor checks the current grant for every
operation and closes the terminal on pause, revocation, or cancellation.

## WSL

Use the Linux Visor runtime and companion inside the active WSL distribution.
Start Visor with the distribution's `WSL_DISTRO_NAME` in its environment and
use a private companion socket inside the distribution. Use a shell from the
same distribution for the CLI, and pass the absolute Linux workspace path.

```sh
hypermesh-companion --terminal-task /home/developer/project context
```

The context includes `wsl_distro` and the workspace. Visor requires the
distribution to match on every terminal request. The existing workspace
assignment, local session approval, and task declaration still apply. If
Visor is stopped or the binding differs, the command reports that the
workspace is unavailable.

In a VS Code WSL window, install Hypermesh in the WSL extension host and set
`hypermesh.companionExecutable` in **Remote Settings** to the Linux companion
path. The terminal task picker shows the distribution, workspace and access.
