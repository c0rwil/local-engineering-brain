export interface TokenPair {
  accessToken: string;
  refreshToken: string;
  expiresAt: number;
}

export class SessionStore {
  private current: TokenPair | null = null;

  read(): TokenPair | null {
    return this.current;
  }

  write(next: TokenPair): void {
    this.current = next;
  }
}

export async function refreshSessionToken(
  store: SessionStore,
  fetcher: typeof fetch,
): Promise<TokenPair> {
  const current = store.read();
  if (!current) {
    throw new Error("no session to refresh");
  }
  const response = await fetcher("/auth/refresh", {
    method: "POST",
    body: JSON.stringify({ refreshToken: current.refreshToken }),
  });
  if (!response.ok) {
    throw new Error(`refresh failed with status ${response.status}`);
  }
  const next = (await response.json()) as TokenPair;
  store.write(next);
  return next;
}
