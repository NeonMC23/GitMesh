# Reports index

These are dated development records. Each one describes the code as it was when it was written. They are
kept for their history and are **not** kept in step with later changes. For how to build, install and
test GitMesh today, use the current guides: [README](../README.md),
[INSTALLATION](../docs/INSTALLATION.md), and [DEVELOPMENT](../docs/DEVELOPMENT.md).

| Report | What it covers | Status (as stated in the report) |
| --- | --- | --- |
| [FINAL_REPORT.md](FINAL_REPORT.md) | The consolidated assessment of the whole first implementation: architecture, capabilities, limitations, debt, security, next steps | Historical. Milestone 10 deliverable |
| [DEVELOPMENT_REPORT.md](DEVELOPMENT_REPORT.md) | The milestone-by-milestone record of the development work | Historical |
| [SETUP_REPORT.md](SETUP_REPORT.md) | Creating a project from the command line or the graphical wizard, with a reviewed plan | Historical. Tracked in Git |
| [REPOSITORY_REPORT.md](REPOSITORY_REPORT.md) | Managing the repositories of an existing project (add, adopt, untrack, rename, remote, remove) | Historical. Tracked in Git |
| [GUI_REPORT.md](GUI_REPORT.md) | The first graphical interface: audit, architecture, implementation, tests | Historical. Superseded in layout by the redesign below |
| [GUI_UX_AUDIT_REPORT.md](GUI_UX_AUDIT_REPORT.md) | GUI and UX audit findings and the improvements made after them | Implemented and validated at the time; **not committed** (untracked) |
| [UX_REDESIGN_AUDIT.md](UX_REDESIGN_AUDIT.md) | The audit that preceded the GUI and terminal interface redesign, written before any implementation change | Audit. **Not committed** (untracked) |
| [UX_REDESIGN_REPORT.md](UX_REDESIGN_REPORT.md) | The implementation report for the redesign (web and terminal interfaces) | Implemented and verified in the working tree at the time; **not committed** (untracked) |
| [LIFECYCLE_REPORT.md](LIFECYCLE_REPORT.md) | Repository lifecycle: clone recovery, remote history, upstream diagnostics, and the documentation pass | Implemented in the working tree; **not committed** (untracked). Records what was and was not verified |
| [INSTALLATION_REPORT.md](INSTALLATION_REPORT.md) | The per-user installation workflow (`install.sh`, its checks, `docs/INSTALLATION.md`), with the validation commands and results | Implemented and validated in the working tree at the time; **not committed** (untracked) |

Two rules apply when reading these records:

* A report's "verified" section lists what was run when it was written. It is not a current test result.
  Current validation is in [DEVELOPMENT.md](../docs/DEVELOPMENT.md).
* Reports are not edited to match later code. If a report and a current guide disagree, the guide is correct.
