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
directly with the listed arguments, closed standard input, and a fresh
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
