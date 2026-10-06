export type Protocol = "openai-chat-completions" | "openai-responses";
export type Capability = { streaming: boolean; tools: boolean; vision: boolean; contextWindowTokens: number | null; maxOutputTokens?: number };
export type TimeoutsMs = { connect: number; firstByte: number; streamIdle: number; total: number };
export type Provider = { protocol: Protocol; baseUrl: string; credentialRef: string; timeoutsMs: TimeoutsMs };
export type AuxiliaryPolicy = "block" | "route-to" | "allow";
export type Config = {
  schemaVersion: number; compatibility: { testedCliVersions: string[]; untestedVersionPolicy: string };
  providers: Record<string, Provider>; models: Array<{ alias: string; displayName: string; provider: string; upstreamModel: string; capabilities: Capability }>;
  routing: { unmatchedGitHubModel: string; unknownCustomModel: string; crossProviderFallback: string; auxiliary: { policy: AuxiliaryPolicy; provider?: string }; githubLeg: { mode: "fail-closed" | "forward" } };
  recording: { defaultMode: string; retentionDays: number; maxTotalMiB: number; maxBodyMiBPerRequest: number; onWriteFailure: string };
};
export type ConfigRevision = Readonly<Config> & { readonly revisionId: number };
export type ValidationError = { path: string; code: string; message: string };
