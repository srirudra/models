export function upstreamHeaders(
  providerHeader: string | undefined,
  credential: string | undefined,
): Record<string, string> {
  const headers: Record<string, string> = {
    accept: "text/event-stream",
    "content-type": "application/json",
  };
  if (credential) {
    headers.authorization = `Bearer ${credential}`;
  }
  if (providerHeader) {
    for (const part of providerHeader.split(",")) {
      const index = part.indexOf("=");
      const name = part.slice(0, index);
      if (index > 0 && /^[a-z0-9-]+$/i.test(name)) {
        headers[name] = part.slice(index + 1);
      }
    }
  }
  return headers;
}
