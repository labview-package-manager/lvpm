# CI in a container (Docker)

lvpm runs unchanged in NI's Windows container image
(`nationalinstruments/labview:latest-windows`, LabVIEW 2026 and later). The
image carries a real LabVIEW install, the registry keys `lvpm targets`
reads, and a `LabVIEW.ini` with VI Server already enabled. The command is
the same one a developer runs in the project, `lvpm install` (a sync of
`lvpm.lock`), followed by whatever LabVIEWCLI operation the job is for.

**Quick start.** Lock on a workstation and commit `lvpm.lock`. The job then
needs nothing but `lvpm sync` (or the bare `lvpm install`): on a headless
machine a missing or out-of-date lock fails the job, so nothing is resolved
quietly against whatever was released since. `lvpm lock --check` makes the
same test without installing anything, for a cheap first step.

```console
> lvpm lock --check     # exit non-zero: lvpm.lock is missing or out of date
> lvpm sync             # install exactly what lvpm.lock lists; --no-dev leaves out the test tooling
```

```dockerfile
FROM nationalinstruments/labview:latest-windows
ENV LV_RTE_HEADLESS=1
COPY lvpm.exe relink-package.vi build-app.vi C:/lvpm/
COPY . C:/src
WORKDIR C:/src
RUN C:\lvpm\lvpm.exe install
RUN LabVIEW.exe "C:\Program Files\National Instruments\LabVIEW 2026\vi.lib\addons\_JKI Toolkits\Caraya\Caraya CLI.vi" -- -s C:\src\Tests -x C:\out\junit.xml
RUN LabVIEWCLI -OperationName MassCompile -DirectoryToCompile C:\src -Headless
```

Measured: the install takes 11 seconds and starts no LabVIEW. Caraya's CLI
then gets a LabVIEW of its own, runs every test under `Tests`, writes JUnit
and quits (121 cases of Caraya's own suite in 90 s). The mass compile takes
47 s.

`LV_RTE_HEADLESS=1` is NI's own switch, not lvpm's. It makes every LabVIEW
start headless: no activation, no dialogs, errors to
`%TEMP%\LabVIEW_*_headless_*_cur.txt` instead. Without it a LabVIEW started
in a container blocks forever on an activation prompt nobody can see. Nobody
sets it on a workstation, so its presence is a reliable sign of an
automation machine with no developer at it, and lvpm reads it as exactly
that. On a headless machine these decisions go the other way, each announced
on one line and each overridable (the relink pass is the same on both: off,
except with `--relink` or for a package marked `relink = true`):

|                                    | developer's machine                                                                 | headless (`LV_RTE_HEADLESS`)                                                                                                         | override                         |
| ---------------------------------- | ----------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------ | -------------------------------- |
| `lvpm.toml` with no venv beside it | refused: a developer inside a project must not install into the machine by accident | the machine is the sandbox: install into the LabVIEW the manifest's `labview` names (a newer one may stand in, an older one may not) | `--global` (with `--labview-version YYYY` to pick the year), `--project` |
| install hooks                      | run only for packages allowed by `hooks = "run"` or `hooks.run`; the rest are skipped with a warning | skipped, and `lvpm list` says so: the hooks measured write a marker file and add a palette entry, IDE furniture                      | `--hooks`                        |
| palettes and menus                 | rebuilt after the install                                                           | not rebuilt: no one sees a palette                                                                                                   | `lvpm refresh`                   |
| `lvpm sync` or bare `lvpm install` | a missing or out-of-date `lvpm.lock` is locked again first                          | acts as `--locked`: a missing or out-of-date `lvpm.lock` fails, after a note saying so. `--frozen` still applies. `lock`, a named `install` and a named `uninstall` are not affected | `--no-locked`                    |

Together those mean a headless `lvpm install` into the LabVIEW installation,
without `--hooks` or `--relink` and with no package marked `relink = true`,
never starts LabVIEW: it is downloads, MD5
checks, file copies and linkfix, and takes seconds. (A headless install into
a venv starts a headless LabVIEW for [the load check](usage.md#the-load-check),
and resaves each package in one only with `--resave`; see
[The resave](usage.md#the-resave).)
Earlier timings on a DQMH project, from before linkfix was the default: two
minutes with hooks, twelve with relink as well, and the mass compile
afterwards reported nothing wrong either way.

A headless machine needs no `--global` to write into a LabVIEW installation,
with or without an `lvpm.toml`: the container's LabVIEW is the sandbox, and
nobody is there to type the flag. A developer's machine does need it
([Writing into LabVIEW itself](usage.md#writing-into-labview-itself)).

The hook skip is the one with a caveat. A hook is arbitrary LabVIEW code,
and a few packages do real setup in theirs. The one hook measured as needed,
NI's Advanced HTTP Client's, is now covered by linkfix
([Script-VI hooks](labview-integration.md#script-vi-hooks)). If a package
behaves differently in the container than on your machine, `--hooks` is the
first thing to try; it starts a headless LabVIEW for them. `--relink`
likewise when the job's output _is_ relinked packages.

NI's Linux image (`nationalinstruments/labview:2026q3-linux`) works the same
way. [Linux](https://github.com/ThatsTheEnd/lvpm/blob/main/docs/linux.md)
has the differences, and every pull request's CI runs lvpm in it.

Two container facts worth knowing. LabVIEW allows one mode at a time per
machine, headless or IDE, so a headless job cannot share a host with an open
IDE. And any LabVIEW that is started (by `--hooks`, `--relink`, `--resave`, or
the job itself) outlives lvpm by design; in a container that keeps a `docker run`
alive, so end such a job with
`LabVIEWCLI -OperationName CloseLabVIEW -Headless` (a Dockerfile `RUN` step
exits on its own). Tools that want a LabVIEW of their own, such as Caraya's
CLI, need none to be running first, which the plain headless install
guarantees.

To keep lvpm's download cache between runs, point `LVPM_CONFIG_CACHE` at a
persistent volume ([Configuration](usage.md#configuration)).
