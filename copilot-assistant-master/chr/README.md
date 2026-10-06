# Copilot Hybrid Router

CHR is a loopback OpenAI-compatible router for Copilot CLI 1.0.88 and 1.0.89. It snapshots configuration per request, resolves registered aliases without prompt inspection, and forwards Chat Completions or Responses SSE incrementally to the configured provider. It binds to IPv4 loopback; IPv6 loopback is intentionally not enabled.
The interceptor mode uses the runtime dependency `node-forge` for ephemeral
certificate generation. It snapshots configuration per request, resolves registered aliases without prompt inspection, and forwards Chat Completions or Responses SSE incrementally to the configured provider. It binds to IPv4 loopback; IPv6 loopback is intentionally not enabled.

Run `npm install`, `npm test`, and `npm run build` from this directory. A configuration uses the schema in `src/config/types.ts`, for example:

```json
{"schemaVersion":1,"compatibility":{"testedCliVersions":["1.0.88","1.0.89"],"untestedVersionPolicy":"block-custom-routing"},"providers":{"local":{"protocol":"openai-chat-completions","baseUrl":"https://example.test/v1","credentialRef":"env:PROVIDER_KEY","timeoutsMs":{"connect":10000,"firstByte":120000,"streamIdle":60000,"total":900000}}},"models":[{"alias":"custom/coding","displayName":"Coding","provider":"local","upstreamModel":"coding-v1","capabilities":{"streaming":true,"tools":true,"vision":false,"contextWindowTokens":null}}],"routing":{"unmatchedGitHubModel":"preserve-original-route","unknownCustomModel":"error","crossProviderFallback":"disabled","auxiliary":{"policy":"block"},"githubLeg":{"mode":"fail-closed"}},"recording":{"defaultMode":"off","retentionDays":7,"maxTotalMiB":1024,"maxBodyMiBPerRequest":16,"onWriteFailure":"continue-inference-and-alert"}}
```

`githubLeg.mode` is deliberately and currently only `fail-closed`. `forward` is rejected with an explicit SEC-15 error because CHR must not call GitHub inference without credential, entitlement, Terms of Service, and legal approval. Auxiliary traffic defaults to `block`: the CLI's observed `gpt-5.4-nano` classifier can receive user prompt content even though the user selected another model, so a conservative benign empty completion prevents an unchosen third-party destination. The policy may instead route to a dedicated configured provider or allow primary routing.

| Requirement | Implementation / tests |
|---|---|
| REG-01–05, REG-10 | `src/config/config.ts`, `tests/core.test.ts`, `tests/config-routing.test.ts` |
| REG-11 | `src/server/server.ts`, `tests/fixtures.integration.test.ts` |
| RTE-01–05, RTE-07, RTE-09–13 | `src/routing/resolve.ts`, `src/routing/classifier.ts`, `tests/config-routing.test.ts` |
| PRO-01–06, PRO-09, PRO-13 | `src/providers/sse.ts`, `src/providers/forward.ts`, `src/server/server.ts`, `tests/sse.test.ts`, `tests/forward.test.ts`, `tests/fixtures.integration.test.ts` |
| SEC-05, SEC-13 | `src/providers/headers.ts`, `src/server/server.ts` |

The forwarding boundary normalizes every emitted SSE frame to LF line endings, one `data: ` prefix per data line, and a terminating blank line. Comments and `event:` fields are intentionally not forwarded because the supported OpenAI-compatible contract consumes data payloads only; data payload content, ordering, UTF-8, tool fragments, and `[DONE]` are preserved. Provider error objects are returned as structured `provider_error` failures.

Provider credentials may use `env:NAME` or `windows-credential:NAME`. The latter is encrypted with
Windows DPAPI for the current Windows user and stored under `%LOCALAPPDATA%\CHR\secrets`.
Manage it without putting the value in command-line arguments:

```
chr secret set CHR_CUSTOM_API_KEY
chr secret set CHR_CUSTOM_API_KEY --from-env CHR_CUSTOM_API_KEY
chr secret list
chr secret remove CHR_CUSTOM_API_KEY
```

`chr secret set` reads the value from standard input (the optional environment import is useful
for migration). Configure a provider with `windows-credential:CHR_CUSTOM_API_KEY`; CHR decrypts
credentials once before launch and caches them in process memory, rather than invoking PowerShell
for every request. DPAPI storage is Windows-only; `env:` remains cross-platform.

## Windows launcher and control CLI

After building, `chr` provides `doctor`, `verify`, `config validate`, `config show`, `launch`, and
`config init`, and `version`. `chr launch` binds CHR to an ephemeral IPv4 loopback port before starting
Copilot, scopes the BYOK environment to that child, forwards terminal I/O and the
child exit code, and shuts the listener down afterward. Use `chr launch -- --help`
to pass arguments to Copilot. Configuration is read from `%LOCALAPPDATA%\CHR\config.json`
or `CHR_CONFIG_PATH`.

Run `chr config init` to create a starter configuration (it refuses to overwrite an
existing file; use `chr config init --force` explicitly). The same secret-free
starter is included as [`config.example.json`](config.example.json); it uses the
placeholder `env:CHR_CUSTOM_API_KEY` and contains no credential value. `doctor` is
read-only: a missing config is reported with the path and the `config init` action,
but does not by itself make doctor fail, because doctor is intended to run before
configuration. A missing or invalid config still blocks `launch`; environment
failures such as an unavailable Copilot executable, blocked tested-version drift,
or loopback bind failure make `doctor` exit non-zero.

The launcher injects a cryptographically random `X-CHR-Session` provider header for
session attribution. The current server API has no seam to validate that token or
map it to request state; it is therefore an attribution hint only until the server
adds authenticated session handling. `doctor` never sends a prompt and redacts
credential references. Existing provider/proxy environment settings are reported
and are overridden only in the launched child process.

`chr verify` runs both the Chat Completions and Responses BYOK probes against an
in-process loopback mock. It accepts `--copilot`, `--timeout`, and `--json`;
provider environment variables are stripped from the child and are not modified
in the parent. Tool definitions are checked for the expected wire shape, but
their count and names are not compatibility invariants because configured MCP
servers can add tools. A passing untested version is reported with a hint to add
it to `testedCliVersions`; verification never edits configuration.

`chr intercept -- <copilot arguments>` launches Copilot through a child-only
HTTPS interceptor. It injects configured chat-completions models into the
GitHub catalog and routes their `/chat/completions` requests to the configured
provider. Responses providers are also advertised and translated for
interception. Other GitHub models and auxiliary requests pass through unchanged;
other CONNECT targets are blind-tunneled. The ephemeral
CA is trusted only by the child process and its public certificate file is
removed when Copilot exits.
Launch model selection: `chr launch --model <alias> -- <copilot arguments>`. With exactly one configured model, `--model` is optional; zero or multiple models require an explicit alias. Launch passes `--model <alias>` to Copilot (and sets `COPILOT_MODEL`), so it overrides a model saved in Copilot settings. A startup banner on stderr identifies the selected alias, upstream model, provider, and loopback URL; it does not include credentials or the session token. The provider protocol maps to `COPILOT_PROVIDER_WIRE_API` (`completions` or `responses`). Launch sets `COPILOT_PROVIDER_TYPE=openai` and selected token limits; `maxOutputTokens` defaults to 16384 and prompt tokens are `contextWindowTokens - maxOutputTokens`. Inherited `COPILOT_PROVIDER_*` and `COPILOT_MODEL` variables are removed before these child-only values are applied. Credentials are resolved by CHR when forwarding and are never passed to Copilot.
