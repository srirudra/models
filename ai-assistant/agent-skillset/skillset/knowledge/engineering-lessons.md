# Engineering Lessons

Record only generalized, evidence-backed lessons that are safe to apply across repositories.

## Entry Template

### `[Lesson title]`

- Guidance: `[Reusable lesson]`
- Applies when: `[Conditions and limits]`
- Evidence pointer: `[Repository and PBI record, without confidential content]`
- Validated: `[ISO date]`
- Status: `Active` or `Superseded by [entry]`

---

### Verify PID identity (start time) before force-killing a tracked process

- Guidance: Persist `{name, pid, process-start-time}` for every process a tool spawns. At stop time, resolve the PID and compare start times before killing; a mismatch means the PID was recycled by an unrelated process. Never kill PID 4 (Windows System) or PIDs whose identity you cannot confirm.
- Applies when: Any supervising CLI manages long-lived child processes across invocations (state file written by `start`, read by a separate `stop` run).
- Evidence pointer: AdaTools — LocalAdvisory `RuntimeStateStore` / `StopCommand` (see in-repo doc `LocalAdvisory/docs/LocalAdvisory-Knowledge.md` §4.3).
- Validated: 2026-08-20
- Status: Active

### Isolate parallel local workspaces with scoped resource names, and keep a port-based force-stop fallback

- Guidance: When one tool can manage multiple side-by-side local environments, scope every shared resource (container names, named volumes, ports where possible) with a workspace-derived prefix, applied idempotently. Pair this with a best-effort force stop that infers known service ports from the declared service commands/launch settings and kills whatever listens on them, so cleanup still works when state files are lost.
- Applies when: Local dev tooling for multi-repo stacks where developers keep several workspaces (e.g. `Test_1`, `Test_3`) running concurrently.
- Evidence pointer: AdaTools — LocalAdvisory container-name patching + `StopCommand` netstat/netsh sweep (in-repo doc §4.3, §5).
- Validated: 2026-08-20
- Status: Active

### Resolve host-dependent config at runtime behind placeholders, and ship a cheap re-apply command

- Guidance: Values that depend on the host (e.g. WSL2 IP, which changes on every reboot) must not be baked in once. Store `${placeholder}` tokens in the config, resolve them at apply time (env-file lookups, `hostname -I`, port detection), warn on unresolved tokens, and provide a fast idempotent `replace`/`apply` command users can re-run after restarts without redoing setup.
- Applies when: Local dev stacks combine WSL/container networking with per-workspace config files pointing at infra (databases, log servers).
- Evidence pointer: AdaTools — LocalAdvisory `replace-variables` command + `ConfigPatcher` (in-repo doc §4.6).
- Validated: 2026-08-20
- Status: Active

### Invoke interactive CLIs without stream redirection; use `where`/`which` for install checks, not `--version`

- Guidance: When a tool shells out to CLIs that may show browser/interactive prompts (e.g. cloud `login`), run them without redirected stdout/stderr (via `cmd /c` on Windows) with a long timeout, so the user can complete the flow; disable upgrade-prompt chatter via the vendor's env var. For "is it installed" checks, prefer `where.exe`/`which` over `<cli> --version`, because version probes with redirected streams can hang when the CLI writes an interactive upgrade prompt to a full pipe buffer.
- Applies when: Wrapping cloud/identity CLI auth flows (Azure, GCP, AWS) inside a higher-level tool.
- Evidence pointer: AdaTools — LocalAdvisory `CloneDbCommand` Azure CLI verification (in-repo doc §4.5).
- Validated: 2026-08-20
- Status: Active

### Legacy .NET Framework web apps under a modern SDK CLI need MSBuild prebuild, VSToolsPath env, and proper web-server config

- Guidance: `dotnet watch run` cannot run classic ASP.NET (`.NET Framework v4.x` web) projects. Detect them (legacy `TargetFrameworkVersion` + web ProjectTypeGuid + `OutputType=Library`), build them first with full MSBuild from a Visual Studio install (injecting `VSToolsPath`/`VisualStudioVersion` so web targets resolve), then host them in IIS Express using the VS-generated `applicationhost.config` if present, else a generated one from the PersonalWebServer template (self-signed 44300–44399 range works out of the box). Also ensure the IIS Express user home config (`Documents\IISExpress\config\aspnet.config`) exists or the managed pool silently degrades to static files only.
- Applies when: A dev tool must support mixed workspaces containing both modern ASP.NET Core and legacy .NET Framework web projects.
- Evidence pointer: AdaTools — LocalAdvisory `ProcessManager` legacy rewrite (in-repo doc §6).
- Validated: 2026-08-20
- Status: Active

### Clone a database by running dump/restore inside the target container with a unique temp archive

- Guidance: To copy a remote DB into a local containerized DB: run `mongodump`/`mongorestore` (or equivalent) *inside* the container via exec; use a unique per-run archive path (guid) under `/tmp` and clean it up; gzip the archive; raise `socketTimeoutMS=0` for WAN dumps; drain stdout to null and stream stderr live so progress is visible; restore against in-container `localhost` (not the host IP); use `--drop` and require an explicit user confirmation first. A `timeout`-wrapped timed dump with `stat` on the archive makes a cheap connection speed test.
- Applies when: Provisioning local data from shared/remote environments into local containers.
- Evidence pointer: AdaTools — LocalAdvisory `CloneDbCommand` (in-repo doc §4.5).
- Validated: 2026-08-20
- Status: Active

### Ship config templates inside a dotnet global tool and apply copy-on-missing setup

- Guidance: A `dotnet tool` that bootstraps workspaces should pack default config templates into the nupkg (`Pack=true`, `PackagePath`) and copy them into the target workspace only when the file does not exist, so re-running setup is idempotent and user edits survive. Resolve template paths from `AppContext.BaseDirectory` with a build-tree fallback for `dotnet run`. Keep per-workspace state/config files out of source control via a generated `.gitignore`.
- Applies when: Building a "setup a local environment" CLI that must be safe to re-run and non-destructive.
- Evidence pointer: AdaTools — LocalAdvisory `Templates/` + `SetupCommand.EnsureTemplate` (in-repo doc §2, §4.1).
- Validated: 2026-08-20
- Status: Active

### Silence CI-only build failures locally with a workspace-root Directory.Build.targets and missing-asset stubs

- Guidance: When local builds fail on CI-only package assets (`EnsureNuGetPackageBuildImports` missing .targets/.props errors, missing analyzer DLLs → CS0006), generate a workspace-root `Directory.Build.targets` (only if absent) that (a) overrides `EnsureNuGetPackageBuildImports` with an empty target and (b) removes `Analyzer` items whose file is missing before `CoreCompile`; plus stub `<Project />` files for each missing import path. This is a local-dev accommodation and must never be committed to the product repos.
- Applies when: Cloning enterprise repos whose full toolchain (CI feeds, analyzers) is not available on a dev machine.
- Evidence pointer: AdaTools — LocalAdvisory `SetupCommand.CreateDirectoryBuildTargets/CreatePackageStubs` (in-repo doc §4.1).
- Validated: 2026-08-20
- Status: Active
